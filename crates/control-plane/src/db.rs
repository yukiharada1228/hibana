//! DB アクセス層 (sqlx ランタイム API のみ; query! マクロは使わない)。
//!
//! 列構成は migrations/0001_init.sql に厳密対応。M1 は単一テナント 'default'。
//! 将来 (M3) のテナント分離に備え、関数は常に `tenant_id` を受け取る。

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

use faas_shared::{ExecutionStatus, Role, Scope, UsageMetrics};

use crate::auth::Principal;

/// リクエスト冒頭でトランザクションにテナントコンテキストを設定する (§3.2)。
///
/// `set_config(..., true)` = SET LOCAL 相当（tx 境界でリセット → プール再利用で残存しない）。
/// RLS ポリシーは `current_setting('app.tenant_id')` をフォールバック無しで参照するため、
/// この呼び出しを忘れた tx 内の全テナントクエリは ERROR で fail-closed する（漏洩しない）。
/// パラメータバインドのみ（SET ステートメントの文字列結合は禁止: injection 防止）。
///
/// `&mut Transaction` は `&mut *tx` で `&mut PgConnection` に deref できるため、handler /
/// worker / subscriber いずれもこの単一シグネチャで呼べる。
pub async fn set_tenant_guc(
    conn: &mut sqlx::PgConnection,
    tenant_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(SET_TENANT_GUC_SQL)
        .bind(tenant_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// `set_tenant_guc` が発行する SQL。tenant_id は `$1` バインドのみで渡し、決して文字列
/// 結合しない（injection 防止）。`true` で transaction-local（SET LOCAL 相当）。
/// 定数として切り出すことで DB 非依存のユニットテストで不変条件を検査できる。
const SET_TENANT_GUC_SQL: &str = "SELECT set_config('app.tenant_id', $1, true)";

/// `components` の 1 行 (API 応答に必要な範囲)。
#[derive(Debug, Clone)]
pub struct ComponentRow {
    pub id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    /// M7a: canary 側のポインタ（migration 0009）。`delete_version` の 3 ポインタ保護に使う。
    pub canary_version_id: Option<String>,
    /// M7a: 直前の stable（引数なし rollback の戻り先）。同じく削除保護の対象。
    pub previous_active_version_id: Option<String>,
}

/// `executions` の 1 行 (GET /executions/{id} 応答)。
#[derive(Debug, Clone)]
pub struct ExecutionRow {
    pub id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<Value>,
    /// M3d (§3.4 / §6.4): 大入力の退避オブジェクトキー（NULL=インライン）。
    pub input_ref: Option<String>,
    /// M3d (§3.4 / §6.4): 大出力の退避オブジェクトキー（NULL=インライン）。
    pub output_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl ExecutionRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            tenant_id: row.try_get("tenant_id")?,
            component_id: row.try_get("component_id")?,
            version_id: row.try_get("version_id")?,
            status: row.try_get("status")?,
            input: row.try_get("input")?,
            output: row.try_get("output")?,
            error: row.try_get("error")?,
            input_ref: row.try_get("input_ref")?,
            output_ref: row.try_get("output_ref")?,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    }
}

/// Component メタデータのみを作成する (§6.2 M2)。
///
/// M2: 初期 version は作らない（active_version_id は NULL のまま）。実体は
/// POST /components/{id}/versions（insert_version）で投入する。
pub async fn create_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO components (id, tenant_id, name, active_version_id) \
         VALUES ($1, $2, $3, NULL)",
    )
    .bind(component_id)
    .bind(tenant_id)
    .bind(name)
    .execute(executor)
    .await?;
    Ok(())
}

/// テナント内で id から component を解決する (soft-delete 済みは除外)。
pub async fn find_component_by_id(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Option<ComponentRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, active_version_id, canary_version_id, previous_active_version_id \
         FROM components \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(ComponentRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            active_version_id: r.try_get("active_version_id")?,
            canary_version_id: r.try_get("canary_version_id")?,
            previous_active_version_id: r.try_get("previous_active_version_id")?,
        })
    })
    .transpose()
}

/// 検証通過した version を登録する (§6.2)。
///
/// 同一 (component_id, version) の重複は UNIQUE 違反（呼び出し側で 409/422 へ）。
/// `storage_uri` は Object Storage 上のオブジェクトキー。`status` は 'active' を渡す
/// （検証通過後に active 化する。pending→active の遷移は M2 では即時）。
/// `size_bytes` は検証で確定した本体サイズ (§6.2)、`capabilities` は §4.4 strict matching を
/// 通過した **承認済み import 集合**（クライアント宣言値ではない）で、migrations/0002_m2.sql の
/// 列に対応する。呼び出し側（handlers）は `Validated::approved_imports` を渡すこと。
#[allow(clippy::too_many_arguments)]
pub async fn insert_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
    version: &str,
    storage_uri: &str,
    wasm_sha256: &str,
    size_bytes: i64,
    capabilities: &Value,
    resource_limits: &Value,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO component_versions \
         (id, component_id, tenant_id, version, storage_uri, wasm_sha256, \
          size_bytes, capabilities, resource_limits, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(version_id)
    .bind(component_id)
    .bind(tenant_id)
    .bind(version)
    .bind(storage_uri)
    .bind(wasm_sha256)
    .bind(size_bytes)
    .bind(capabilities)
    .bind(resource_limits)
    .bind(status)
    .execute(executor)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// M7a: stable ポインタを動かす 3 操作（switch / promote / rollback）と canary 配分。
//
// **3 関数共通の不変条件**:
//  (a) 必ず**単一 UPDATE 文**で完結する（「重みだけ 0 になって active は新版のまま」といった
//      中間状態が観測されない）。
//  (b) 必ず canary をクリアする（`canary_version_id = NULL`, `canary_weight = 0`）。
//      これにより「新しい stable を入れたのに古い canary 配分が残って混線する」が構造的に起きない。
//      canary 未使用時のクリアは no-op なので既存 API の挙動は不変。
//  (c) `previous_active_version_id` は **CASE ガード**付きで更新する。同じ版を再 activate
//      （宣言的 CI/CD が毎回同じ version を PUT する運用）したときに previous を自分自身で
//      潰さない。潰すと直後の rollback が「200 を返すのに何も戻らない」最悪の failure mode になる。
// ---------------------------------------------------------------------------

/// stable ポインタを切り替える (§6.7)。M7a: 直前 stable を退避し、canary 配分を必ずクリアする。
///
/// 戻り値は更新が起きたか。既存の呼び出し 2 箇所（`upload_version` / `set_active_version` ハンドラ）は
/// 同一 tx 内で先に `find_component_by_id` で 404 判定済みなので `false` は到達しない防御。
/// bool を返すのは promote / rollback が事前 SELECT 無しの単一 UPDATE で 0 行を 404/409/400 へ
/// 写像する必要があり、3 関数の形を揃えるため。
pub async fn switch_active_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(SWITCH_ACTIVE_VERSION_SQL)
        .bind(tenant_id)
        .bind(component_id)
        .bind(version_id)
        .execute(executor)
        .await?;
    Ok(r.rows_affected() > 0)
}

const SWITCH_ACTIVE_VERSION_SQL: &str = "UPDATE components \
        SET previous_active_version_id = CASE \
                WHEN active_version_id IS DISTINCT FROM $3 THEN active_version_id \
                ELSE previous_active_version_id END, \
            active_version_id = $3, \
            canary_version_id = NULL, \
            canary_weight = 0, \
            canary_updated_at = now() \
      WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL";

/// canary を stable へ昇格する (M7a)。**単一 UPDATE + CAS**（read-then-write のレースを作らない）。
///
/// `$3`(target) が NULL なら現 `canary_version_id` を昇格。非 NULL なら「オペレータが見た canary と
/// 昇格対象が一致すること」を CAS 条件にする（別 CP の `PUT /traffic` が割り込んで別の版を 100%
/// 出す事故を防ぐ）。0 行 = component 不在 / canary 未設定 / CAS 不一致。
const PROMOTE_ACTIVE_VERSION_SQL: &str = "UPDATE components \
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

/// canary を stable へ昇格する。成功時は `(新 active_version_id, 旧 active_version_id)` を返す。
///
/// 0 行（`None`）の理由確定（component 不在 / canary 未設定 / CAS 不一致）は呼び出し側が
/// **0 行のときだけ**再 SELECT して行う。成功パスは 1 文のままなのでレースは起きない。
pub async fn promote_active_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    target_version_id: Option<&str>,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    let row = sqlx::query(PROMOTE_ACTIVE_VERSION_SQL)
        .bind(tenant_id)
        .bind(component_id)
        .bind(target_version_id)
        .fetch_optional(executor)
        .await?;
    row.map(|r| {
        Ok((
            r.try_get("active_version_id")?,
            r.try_get("previous_active_version_id")?,
        ))
    })
    .transpose()
}

/// ワンクリック rollback: canary を破棄し、指定版（NULL なら `previous_active_version_id`）へ戻す。
///
/// **戻り先は必ず「未削除の version」として解決する**。単なる COALESCE UPDATE にすると previous が
/// soft delete 済みのとき tombstone を黙って active にしてしまう（解決 SQL の stable 側は
/// `deleted_at` を見ないので、`GET /components/{id}/versions` に出てこない版が 100% を受ける幽霊状態に
/// なる）。FROM 句の JOIN で `cv.deleted_at IS NULL` を要求し、満たさなければ 0 行 → 409 に倒す。
const ROLLBACK_ACTIVE_VERSION_SQL: &str = "UPDATE components c \
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

/// ワンクリック rollback。成功時は `(新 active_version_id, 旧 active_version_id)` を返す。
///
/// 0 行（`None`）は component 不在 / previous が NULL / 戻り先が soft delete 済み。理由確定は
/// 呼び出し側が 0 行のときだけ再 SELECT する。
pub async fn rollback_active_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    target_version_id: Option<&str>,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    let row = sqlx::query(ROLLBACK_ACTIVE_VERSION_SQL)
        .bind(tenant_id)
        .bind(component_id)
        .bind(target_version_id)
        .fetch_optional(executor)
        .await?;
    row.map(|r| {
        Ok((
            r.try_get("active_version_id")?,
            r.try_get("previous_active_version_id")?,
        ))
    })
    .transpose()
}

/// canary の版と重みを設定する（絶対値の PUT。冪等）。戻り値は更新が起きたか。
///
/// `version_id` の存在検証（当該 component の未削除 version であること）は
/// **アプリ側（`find_version_id`）が唯一の参照整合**である（`canary_version_id` に FK は張らない）。
/// 万一壊れたポインタが入っても解決 SQL の LEFT JOIN が外れて全量 stable に倒れる（fail-safe）。
pub async fn set_traffic_split(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
    weight: i16,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE components \
            SET canary_version_id = $3, canary_weight = $4, canary_updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(version_id)
    .bind(weight)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// `GET /components/{id}/traffic` が返す現在の配分（版は semver も解決済み）。
#[derive(Debug, Clone)]
pub struct TrafficSplitRow {
    pub component_id: String,
    pub stable_version_id: Option<String>,
    pub stable_version: Option<String>,
    pub canary_version_id: Option<String>,
    pub canary_version: Option<String>,
    pub weight: i16,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// component id から現在の traffic 配分を引く（`GET /components/{id}/traffic`）。
///
/// 解決 SQL（`RESOLVE_ROUTING_SQL`）と違い **active version が無くても行を返す**（未デプロイの
/// component でも 200 で「stable=null / weight=0」を返したい）。canary 側は解決 SQL と同じ 4 条件で
/// LEFT JOIN するため、soft delete 済み / 不整合ポインタは `canary_version = null` として見える
/// （＝ 実際のルーティングと同じものが観測できる）。
pub async fn traffic_split_for_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Option<TrafficSplitRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT c.id AS component_id, c.canary_weight, c.canary_updated_at, \
                sv.id AS stable_version_id, sv.version AS stable_version, \
                cvv.id AS canary_version_id, cvv.version AS canary_version \
           FROM components c \
           LEFT JOIN component_versions sv \
             ON sv.id = c.active_version_id AND sv.tenant_id = c.tenant_id \
            AND sv.component_id = c.id \
           LEFT JOIN component_versions cvv \
             ON cvv.id = c.canary_version_id AND cvv.tenant_id = c.tenant_id \
            AND cvv.component_id = c.id AND cvv.deleted_at IS NULL \
          WHERE c.tenant_id = $1 AND c.id = $2 AND c.deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(TrafficSplitRow {
            component_id: r.try_get("component_id")?,
            stable_version_id: r.try_get("stable_version_id")?,
            stable_version: r.try_get("stable_version")?,
            canary_version_id: r.try_get("canary_version_id")?,
            canary_version: r.try_get("canary_version")?,
            weight: r.try_get("canary_weight")?,
            updated_at: r.try_get("canary_updated_at")?,
        })
    })
    .transpose()
}

/// version 別の直近ウィンドウ成績（canary の go/no-go 判断の一次情報）。
#[derive(Debug, Clone)]
pub struct VersionStatsRow {
    pub version_id: String,
    pub succeeded: i64,
    pub failed: i64,
    pub timeout: i64,
    pub canary_routed: i64,
    /// `wall_time_ms` が全 NULL の版（DLQ / timeout のみ）では percentile が NULL になる。
    pub p50_wall_time_ms: Option<i64>,
    pub p95_wall_time_ms: Option<i64>,
}

/// 直近 `window_minutes` 分の終端実行を version 別に集計する (M7a の観測)。
///
/// `usage_rollups`（M5）は PK に version 次元が無く、粒度も日次なので canary 判断には使えない。
/// M5 の集計は 1 行も変えず、`executions` 生表を直近ウィンドウで直読みする（0009 で追加した
/// 部分 index `idx_executions_component_finished` がこの走査に対応する）。
pub async fn version_stats_for_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    window_minutes: i32,
) -> Result<Vec<VersionStatsRow>, sqlx::Error> {
    let rows = sqlx::query(VERSION_STATS_SQL)
        .bind(tenant_id)
        .bind(component_id)
        .bind(window_minutes)
        .fetch_all(executor)
        .await?;

    rows.into_iter()
        .map(|r| {
            Ok(VersionStatsRow {
                version_id: r.try_get("version_id")?,
                succeeded: r.try_get("succeeded")?,
                failed: r.try_get("failed")?,
                timeout: r.try_get("timeout")?,
                canary_routed: r.try_get("canary_routed")?,
                p50_wall_time_ms: r.try_get("p50_wall_time_ms")?,
                p95_wall_time_ms: r.try_get("p95_wall_time_ms")?,
            })
        })
        .collect()
}

const VERSION_STATS_SQL: &str = "SELECT version_id, \
       COUNT(*) FILTER (WHERE status = 'succeeded')::bigint AS succeeded, \
       COUNT(*) FILTER (WHERE status = 'failed')::bigint    AS failed, \
       COUNT(*) FILTER (WHERE status = 'timeout')::bigint   AS timeout, \
       COUNT(*) FILTER (WHERE routing_reason = 'canary')::bigint AS canary_routed, \
       percentile_cont(0.5)  WITHIN GROUP (ORDER BY wall_time_ms)::bigint AS p50_wall_time_ms, \
       percentile_cont(0.95) WITHIN GROUP (ORDER BY wall_time_ms)::bigint AS p95_wall_time_ms \
  FROM executions \
 WHERE tenant_id = $1 AND component_id = $2 \
   AND status IN ('succeeded', 'failed', 'timeout') \
   AND finished_at >= now() - ($3::int * interval '1 minute') \
 GROUP BY version_id";

/// canary を解除する（`canary_version_id = NULL`, `canary_weight = 0`）。既に未設定でも成功（冪等）。
pub async fn clear_traffic_split(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE components \
            SET canary_version_id = NULL, canary_weight = 0, canary_updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// invoke 用に解決した active version の保存先情報。
#[derive(Debug, Clone)]
pub struct ActiveVersion {
    pub version_id: String,
    pub version: String,
    /// Object Storage 上のオブジェクトキー。
    pub storage_uri: String,
    pub wasm_sha256: String,
    /// M3c: token exp 計算に使う壁時計上限（ms）。resource_limits(JSONB) から解決する。
    pub max_wall_time_ms: u64,
}

/// 解決 SQL (M7a)。stable / canary の両側で「id 一致 + tenant 一致 + component 一致」を JOIN 条件にする。
///
/// **`component_id = c.id` を両側に入れる理由**: ポインタが同一テナント内の別 component の version を
/// 指してしまった場合（運用の手 UPDATE / バックアップ復元 / component 削除→再作成）、これが無いと
/// `JobMessage` が `component=A` / `version=<B の semver>` になり、worker の `resolve_limits` が
/// `(tenant, component 名, version)` で行を引けず `ResourceLimits::default()` へ**無言でフォールバック**
/// する（＝承認外のリソース上限で実行される）。さらに `executions.component_id` と `version_id` が
/// 食い違って M5 の課金帰属も壊れる。**別 component の wasm を実行するより、起動しないほうが安全**。
/// 一貫したデータでは結果が変わらない厳密な絞り込みであり、壊れたポインタのときだけ
/// 「行なし → 既存の 400 `has no active version`」へ倒れる。
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

/// `resource_limits`(JSONB) から壁時計上限を解決する。欠損 / 不正は既定にフォールバック。
fn max_wall_time_from_limits(limits_json: Value) -> u64 {
    serde_json::from_value::<faas_shared::ResourceLimits>(limits_json)
        .unwrap_or_default()
        .max_wall_time_ms
}

/// component 名から stable / canary の両ポインタを 1 本の SQL で解決する (M7a, §6.7 / §15)。
///
/// `active_version_id` が NULL のとき（version 未投入）や、stable ポインタが壊れているときは `None`
/// ＝ 呼び出し側は従来どおり 400 `has no active version` / cron の advance-only skip に倒れる。
///
/// canary 側は LEFT JOIN の 4 条件（id / tenant / component / 未削除）が揃ったときだけ `Some` になる。
/// どれか 1 つでも外れれば `None` ＝ **重みに関わらず全量 stable**（fail-safe）。
///
/// 呼び出しは必ず `db::set_tenant_guc` 済みの tx から行う（RLS の二重防御: GUC + `WHERE tenant_id = $1`）。
pub async fn resolve_component_routing(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_name: &str,
) -> Result<Option<crate::routing::ComponentRouting>, sqlx::Error> {
    let Some(r) = sqlx::query(RESOLVE_ROUTING_SQL)
        .bind(tenant_id)
        .bind(component_name)
        .fetch_optional(executor)
        .await?
    else {
        return Ok(None);
    };

    let stable = ActiveVersion {
        version_id: r.try_get("stable_version_id")?,
        version: r.try_get("stable_version")?,
        storage_uri: r.try_get("stable_storage_uri")?,
        wasm_sha256: r.try_get("stable_wasm_sha256")?,
        max_wall_time_ms: max_wall_time_from_limits(r.try_get("stable_resource_limits")?),
    };

    // LEFT JOIN が外れた場合は canary_version_id が NULL で返る（= fail-safe に全量 stable）。
    let canary_version_id: Option<String> = r.try_get("canary_version_id")?;
    let canary = match canary_version_id {
        Some(version_id) => Some(ActiveVersion {
            version_id,
            version: r.try_get("canary_version")?,
            storage_uri: r.try_get("canary_storage_uri")?,
            wasm_sha256: r.try_get("canary_wasm_sha256")?,
            max_wall_time_ms: max_wall_time_from_limits(r.try_get("canary_resource_limits")?),
        }),
        None => None,
    };

    // DB の CHECK（migration 0009）で 0..=100 は保証されるが、防御的に clamp して読む。
    let weight: i16 = r.try_get("canary_weight")?;
    let canary_weight = weight.clamp(0, 100) as u8;

    Ok(Some(crate::routing::ComponentRouting {
        component_id: r.try_get("component_id")?,
        stable,
        canary,
        canary_weight,
    }))
}

/// テナント内で name から component を解決する (soft-delete 済みは除外)。
pub async fn find_component_by_name(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    name: &str,
) -> Result<Option<ComponentRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, active_version_id, canary_version_id, previous_active_version_id \
         FROM components \
         WHERE tenant_id = $1 AND name = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(name)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(ComponentRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            active_version_id: r.try_get("active_version_id")?,
            canary_version_id: r.try_get("canary_version_id")?,
            previous_active_version_id: r.try_get("previous_active_version_id")?,
        })
    })
    .transpose()
}

/// 実行を pending で INSERT する (§ /invoke)。
///
/// 冪等列を持たない簡易版（`insert_pending_execution_with_provenance(.., None, None, None)`
/// に委譲）。M3c 以降の invoke は provenance 版を直接使うため現状この簡易版に呼び出しは
/// 無いが、冪等/トークンを伴わない将来の挿入経路・テスト向けに API として残す。
#[allow(dead_code)]
pub async fn insert_pending_execution(
    executor: impl sqlx::PgExecutor<'_>,
    execution_id: &str,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
    input: &Value,
) -> Result<(), sqlx::Error> {
    insert_pending_execution_with_provenance(
        executor,
        execution_id,
        tenant_id,
        component_id,
        version_id,
        input,
        None,
        None,
        None,
        None,
        0,
        // M7a: この簡易版は canary 解決を通らない（テスト / 将来の非 invoke 経路）ため
        // 定義上 stable 相当。executions_routing_reason_chk の値域に合わせる。
        crate::routing::RoutingReason::Stable.as_str(),
    )
    .await
}

/// 実行を pending で INSERT する（M3c: 冪等性 + トークン出所の列を併せて保存する）。
///
/// `idempotency_key` は §6.6 の冪等キー（NULL なら無制約）、`idempotency_request_hash` は
/// canonical invoke body の sha256 hex（同一 key の body 不一致 → 409 判定に使う不変条件）、
/// `job_token_kid` は発行した署名トークンの kid（rotation 観測用; トークン本体は保存しない）。
/// `input_ref` は大入力の退避オブジェクトキー（§3.4 / §6.4。NULL=インライン入力。値は
/// `tenants/{tenant}/io/{execution_id}/input` に完全一致したものだけが呼び出し側で渡される）。
/// 冪等列は migrations/0005_provenance.sql、input_ref は migrations/0006_large_io.sql の
/// nullable 列に対応する。
/// `routing_reason` は M7a の version 決定理由（`"stable"` | `"canary"`, migration 0009）。
/// `components` は可変なので version_id だけでは昇格後に stable/canary の区別が失われる。
/// 実行時点のスナップショットとしてここで固定する（`routing::RoutingReason::as_str()` を渡す）。
///
/// 部分 UNIQUE (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL に違反すると
/// 23505 を返す。並行同一 key の二重 INSERT 競合では呼び出し側がこれを捕捉し、再 SELECT して
/// 同一 body→既存返却 / 異 body→409 を判定する（index が権威; SELECT は fast path）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_pending_execution_with_provenance(
    executor: impl sqlx::PgExecutor<'_>,
    execution_id: &str,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    idempotency_request_hash: Option<&str>,
    job_token_kid: Option<&str>,
    input_ref: Option<&str>,
    chain_depth: i32,
    routing_reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO executions \
         (id, tenant_id, component_id, version_id, status, input, \
          idempotency_key, idempotency_request_hash, job_token_kid, input_ref, chain_depth, \
          routing_reason) \
         VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(execution_id)
    .bind(tenant_id)
    .bind(component_id)
    .bind(version_id)
    .bind(input)
    .bind(idempotency_key)
    .bind(idempotency_request_hash)
    .bind(job_token_kid)
    .bind(input_ref)
    .bind(chain_depth)
    .bind(routing_reason)
    .execute(executor)
    .await?;
    Ok(())
}

/// 実行行の `chain_depth`（Component チェーンのホップ深さ, §15 M6c）を引く。
///
/// chain トリガーの終端成功フックが「上流の深さ+1 が `MAX_CHAIN_DEPTH` を超えるか」を判定するために
/// 使う。行不在（既に GC 済み等）は `None`。RLS 下で呼ぶ（GUC 済み tx）。
pub async fn execution_chain_depth(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    let row = sqlx::query("SELECT chain_depth FROM executions WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(execution_id)
        .fetch_optional(executor)
        .await?;
    row.map(|r| r.try_get::<i32, _>("chain_depth")).transpose()
}

/// 冪等キーで既存実行を引く（§6.6 layer 1 fast path / 23505 後の再解決）。
///
/// `(tenant_id, idempotency_key)` で一意（部分 UNIQUE）。見つかれば `(execution_id, status,
/// idempotency_request_hash)` を返し、呼び出し側が `request_hash` を現リクエストの hash と
/// 突き合わせて「同一 body → 既存を返す / 異 body → 409」を決める。`request_hash` は
/// 列が NULL の理論上の行に備え `Option` で返す（通常はキー保存時に必ず併存する）。
pub async fn find_execution_by_idempotency_key(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    idempotency_key: &str,
) -> Result<Option<(String, String, Option<String>)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, status, idempotency_request_hash FROM executions \
         WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant_id)
    .bind(idempotency_key)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok((
            r.try_get::<String, _>("id")?,
            r.try_get::<String, _>("status")?,
            r.try_get::<Option<String>, _>("idempotency_request_hash")?,
        ))
    })
    .transpose()
}

/// subscriber のトークン claim 突き合わせ用に、実行行の最小情報を引く（M3c）。
///
/// 署名 claim（execution_id / tenant_id / version_id）を行の権威値と照合するため、
/// `(component_id, version_id, status)` を返す（行不在は `None` → 「未知の execution」として
/// drop+audit）。tenant_id は呼び出し側が GUC（subject 由来テナント）で既に拘束しているため返さない。
///
/// M5 (§15): `component_id` は finalize と同一 tx で打つ `usage_rollups` の集計キー
/// （テナント×UTC日×component）に必要なため同一行から追加で引く（同じ行参照なので追加コスト最小・
/// RLS 下でも副作用なし）。
pub async fn find_execution_provenance(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<(String, String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT component_id, version_id, status FROM executions \
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok((
            r.try_get::<String, _>("component_id")?,
            r.try_get::<String, _>("version_id")?,
            r.try_get::<String, _>("status")?,
        ))
    })
    .transpose()
}

/// version_id から `ResourceLimits`（JSONB）を解決する (M5, §15)。
///
/// finalize 経路で worker 自己申告の計量を sanity clamp する上限を引くために使う（worker の
/// `get_limits` と同じ `component_versions.resource_limits` を参照する＝同一権威値で clamp する）。
/// 行不在は `None`、JSONB のパース失敗時は `ResourceLimits::default()` にフォールバックする
/// （壁時計上限の解決と同じ `unwrap_or_default` 規約）。
pub async fn find_version_resource_limits(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    version_id: &str,
) -> Result<Option<faas_shared::ResourceLimits>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT resource_limits FROM component_versions \
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(version_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        let limits_json: Value = r.try_get("resource_limits")?;
        Ok(serde_json::from_value::<faas_shared::ResourceLimits>(limits_json).unwrap_or_default())
    })
    .transpose()
}

/// 実行を取得する。
pub async fn get_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<ExecutionRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, tenant_id, component_id, version_id, status, input, output, error, \
                input_ref, output_ref, created_at, started_at, finished_at \
         FROM executions WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await?;

    row.as_ref().map(ExecutionRow::from_row).transpose()
}

// ---------------------------------------------------------------------------
// Component / Version 管理 (§6.7)
// ---------------------------------------------------------------------------

/// `GET /components` 応答用の 1 行。
#[derive(Debug, Clone)]
pub struct ComponentListItem {
    pub component_id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// テナントの Component 一覧 (soft-delete 済みは除外, §6.7)。新しい順。
pub async fn list_components(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<Vec<ComponentListItem>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, active_version_id, created_at FROM components \
         WHERE tenant_id = $1 AND deleted_at IS NULL \
         ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(ComponentListItem {
                component_id: r.try_get("id")?,
                name: r.try_get("name")?,
                active_version_id: r.try_get("active_version_id")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

/// `GET /components/{id}/versions` 応答用の 1 行。
#[derive(Debug, Clone)]
pub struct VersionListItem {
    pub version_id: String,
    pub version: String,
    pub status: String,
    pub size_bytes: i64,
    pub wasm_sha256: String,
    pub created_at: DateTime<Utc>,
}

/// 当該 component の version 一覧 (soft-delete 済みは除外, §6.7)。新しい順。
pub async fn list_versions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<VersionListItem>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, version, status, size_bytes, wasm_sha256, created_at \
         FROM component_versions \
         WHERE tenant_id = $1 AND component_id = $2 AND deleted_at IS NULL \
         ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(VersionListItem {
                version_id: r.try_get("id")?,
                version: r.try_get("version")?,
                status: r.try_get("status")?,
                size_bytes: r.try_get("size_bytes")?,
                wasm_sha256: r.try_get("wasm_sha256")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

/// component を soft delete する (deleted_at=now(), §6.7)。
///
/// 既に削除済み / 不在の場合は更新 0 行。呼び出し側は事前に存在確認・参照保護を行う。
pub async fn soft_delete_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE components SET deleted_at = now() \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// 当該 component の (version 文字列) から未削除の version の `id` (`ver_*`) を解決する (§6.7)。
///
/// soft delete / active-version 切替の対象確認に使う。呼び出し側が必要とするのは
/// version_id のみのため、行全体ではなく id を返す。
pub async fn find_version_id(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id FROM component_versions \
         WHERE tenant_id = $1 AND component_id = $2 AND version = $3 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(version)
    .fetch_optional(executor)
    .await?;

    row.map(|r| r.try_get::<String, _>("id")).transpose()
}

/// version の `capabilities` JSONB を引く（M7b: env 許可リストの解決）。
pub async fn version_capabilities(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    version_id: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT capabilities FROM component_versions \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(version_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| r.try_get::<Value, _>("capabilities"))
        .transpose()
}

/// version の `capabilities.env`（注入を許可する env 名）を全置換する (M7b, §4.4)。
///
/// **admin 承認の唯一の書き込み点**。`imports` 側（strict matching の結果）は保持したまま
/// `env` キーだけを差し替える（jsonb_set 相当をアプリ側で組み立てて渡す）。
/// 戻り値は更新が起きたか（0 行 = version 不在 / soft delete 済み → 呼び出し側が 404）。
pub async fn set_version_capabilities(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    version_id: &str,
    capabilities: &Value,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE component_versions SET capabilities = $3 \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(version_id)
    .bind(capabilities)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// M7b: per-function の平文環境変数（function_configs, migration 0010）
//
// **平文である**ことが型と権限の両方で分かるようにする（secret は function_secrets 側で
// 暗号化して持ち、読み出し API を持たない）。すべて set_tenant_guc 済み tx で呼ぶこと。
// ---------------------------------------------------------------------------

/// `function_configs` の 1 行（平文なので値も返す）。
#[derive(Debug, Clone)]
pub struct FunctionConfigRow {
    pub key: String,
    pub value: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// per-function config を 1 キー upsert する。
pub async fn upsert_function_config(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    key: &str,
    value: &str,
    updated_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_configs (tenant_id, component_id, key, value, updated_by) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, component_id, key) \
         DO UPDATE SET value = EXCLUDED.value, updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(key)
    .bind(value)
    .bind(updated_by)
    .execute(executor)
    .await?;
    Ok(())
}

/// component の config を全件引く（キー名昇順 ＝ 応答と注入の決定性）。
pub async fn list_function_configs(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<FunctionConfigRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT key, value, updated_at FROM function_configs \
          WHERE tenant_id = $1 AND component_id = $2 ORDER BY key",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(FunctionConfigRow {
                key: r.try_get("key")?,
                value: r.try_get("value")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}

/// config を 1 キー削除する。戻り値は削除が起きたか（0 行 → 404）。
pub async fn delete_function_config(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "DELETE FROM function_configs WHERE tenant_id = $1 AND component_id = $2 AND key = $3",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(key)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// component の config を全置換する（`PUT /config` のトランザクション内で使う）。
///
/// 「全置換」を DELETE + INSERT で表現する。同一 tx 内なので中間状態は観測されない。
pub async fn replace_function_configs(
    tx: &mut sqlx::PgConnection,
    tenant_id: &str,
    component_id: &str,
    entries: &[(String, String)],
    updated_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM function_configs WHERE tenant_id = $1 AND component_id = $2")
        .bind(tenant_id)
        .bind(component_id)
        .execute(&mut *tx)
        .await?;
    for (key, value) in entries {
        upsert_function_config(&mut *tx, tenant_id, component_id, key, value, updated_by).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// M7c: Secrets Manager（function_secrets / function_secret_versions, migration 0011）
//
// **平文も暗号文もこの層より上へ素で流さない**。値は `secrets.rs` の封筒 [`crate::secrets::Envelope`]
// としてのみ受け渡す。版台帳は追記専用（faas_app に UPDATE/DELETE が無い）なので、値の更新
// （rotate）も KEK 再ラップ（rekey）も「新しい version 行の INSERT」で表現する。
// すべて set_tenant_guc 済み tx で呼ぶこと。
// ---------------------------------------------------------------------------

/// `function_secrets` のメタデータ 1 行（**値は含まない**）。
#[derive(Debug, Clone)]
pub struct SecretMetaRow {
    pub id: String,
    pub name: String,
    pub current_version: i32,
    /// 作成時刻。M7c-3 の execution 基準の世代解決（§4.7）で参照する。
    #[allow(dead_code)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// secret メタ行を作成する（版行は別途 `insert_secret_version` で INSERT する）。
///
/// 生存行の同名重複は部分 UNIQUE index 違反（23505）。呼び出し側が捕捉して
/// 「既存 → rotate」へ倒す。
pub async fn insert_secret_meta(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    component_id: &str,
    name: &str,
    current_version: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_secrets \
         (id, tenant_id, component_id, name, current_version) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(secret_id)
    .bind(tenant_id)
    .bind(component_id)
    .bind(name)
    .bind(current_version)
    .execute(executor)
    .await?;
    Ok(())
}

/// 生存している secret を名前で引く（**メタのみ**）。
pub async fn find_live_secret_by_name(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<Option<SecretMetaRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, current_version, created_at, updated_at FROM function_secrets \
          WHERE tenant_id = $1 AND component_id = $2 AND name = $3 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(name)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(SecretMetaRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            current_version: r.try_get("current_version")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
        })
    })
    .transpose()
}

/// component の生存 secret を全件引く（**メタのみ**。値も value_len も kek_kid も返さない）。
pub async fn list_secrets_meta(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<SecretMetaRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, current_version, created_at, updated_at FROM function_secrets \
          WHERE tenant_id = $1 AND component_id = $2 AND deleted_at IS NULL ORDER BY name",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(SecretMetaRow {
                id: r.try_get("id")?,
                name: r.try_get("name")?,
                current_version: r.try_get("current_version")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}

/// 版台帳へ 1 行 INSERT する（追記専用）。`reason` は `'create' | 'rotate' | 'rekey'`。
pub async fn insert_secret_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
    env: &crate::secrets::Envelope,
    reason: &str,
    created_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_secret_versions \
         (tenant_id, secret_id, version, kek_kid, wrapped_dek, dek_nonce, nonce, ciphertext, \
          value_len, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .bind(&env.kek_kid)
    .bind(&env.wrapped_dek)
    .bind(&env.dek_nonce)
    .bind(&env.nonce)
    .bind(&env.ciphertext)
    .bind(env.value_len)
    .bind(reason)
    .bind(created_by)
    .execute(executor)
    .await?;
    Ok(())
}

/// `current_version` を前進させる（rotate / rekey 後の切替）。
pub async fn bump_secret_current_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE function_secrets SET current_version = $3, updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// secret を soft delete する。版台帳は残る（追記専用なので消せない ＝ 監査上も残す）。
///
/// 生存行の部分 UNIQUE index から外れるため、**同名で作り直せる**（インシデント対応の基本操作）。
pub async fn soft_delete_secret(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE function_secrets SET deleted_at = now(), updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// 指定 secret の指定版の封筒を引く。
pub async fn find_secret_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<Option<crate::secrets::Envelope>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT kek_kid, wrapped_dek, dek_nonce, nonce, ciphertext, value_len \
           FROM function_secret_versions \
          WHERE tenant_id = $1 AND secret_id = $2 AND version = $3",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(crate::secrets::Envelope {
            kek_kid: r.try_get("kek_kid")?,
            wrapped_dek: r.try_get("wrapped_dek")?,
            dek_nonce: r.try_get("dek_nonce")?,
            nonce: r.try_get("nonce")?,
            ciphertext: r.try_get("ciphertext")?,
            value_len: r.try_get("value_len")?,
        })
    })
    .transpose()
}

/// version を soft delete する (deleted_at=now(), §6.7)。
///
/// 既に削除済み / 不在の場合は更新 0 行。active version 保護・参照保護は呼び出し側で行う。
pub async fn soft_delete_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE component_versions SET deleted_at = now() \
         WHERE tenant_id = $1 AND component_id = $2 AND id = $3 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(version_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// 当該 component を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS ( \
            SELECT 1 FROM executions \
            WHERE tenant_id = $1 AND component_id = $2 \
              AND status IN ('pending', 'running') \
         ) AS present",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

/// 当該 version を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    version_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS ( \
            SELECT 1 FROM executions \
            WHERE tenant_id = $1 AND version_id = $2 \
              AND status IN ('pending', 'running') \
         ) AS present",
    )
    .bind(tenant_id)
    .bind(version_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

/// M4d (§8): テナント別クォータ上書き値。`tenants.quotas` JSONB から読む。
///
/// 値がキーごとに `Option<u64>` なのは、欠損 / null を「グローバル既定を継承」として
/// 扱うため（仕様 §8: グローバル既定 → テナント上書きの優先順位で解決）。未知キー
/// （例: 将来追加されるクォータ）は無視する（前方互換）。serde は `deny_unknown_fields`
/// を **使わない** —— 将来追加されたフィールドで起動中の CP が解釈エラーを返さない
/// よう緩く受ける（migrations/0006 のコメント済み例: invoke_rate_per_sec / invoke_burst /
/// max_concurrent_executions）。
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct TenantQuotaOverrides {
    /// invoke レート（req/秒）の上書き。
    #[serde(default)]
    pub invoke_rate_per_sec: Option<u64>,
    /// token-bucket バースト容量の上書き。
    #[serde(default)]
    pub invoke_burst: Option<u64>,
    /// in-flight 同時実行（pending+running）の上書き。
    #[serde(default)]
    pub max_concurrent_executions: Option<u64>,
}

/// M4d (§3.2): テナントの実行ステータス（active|suspended）+ クォータ上書き JSONB。
///
/// invoke 受付フローは「(1) suspended 判定で 403、(2) override をグローバル既定にマージ」
/// の 2 つを **同じ row** から導出する。クエリを 1 度にまとめてホットパスのラウンドトリップを
/// 削る（PK lookup なので sub-ms）。tenants には RLS が無い（tenant_id 列を持たない）ため、
/// 呼び出し側で `app.tenant_id` GUC をセットする必要はない（faas_app は SELECT 権限を持つ）。
///
/// 存在しないテナント id（理論上、認証後の principal なので発生しない）は `None`。
/// JSONB が壊れている場合は **エラー伝播せず**、`quotas = TenantQuotaOverrides::default()` に
/// 縮退する（fail-open: 既定値で動かす方が観測しやすい。warn ログを残す）。
pub async fn load_tenant_status_and_quotas(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<Option<(String, TenantQuotaOverrides)>, sqlx::Error> {
    let row = sqlx::query("SELECT status, quotas FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(executor)
        .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    let status: String = r.try_get("status")?;
    let quotas_json: Value = r.try_get("quotas")?;
    // 壊れた JSON は既定に縮退する（CP を壊さない）。タイプ不一致（"50" など）も同様。
    let quotas: TenantQuotaOverrides = match serde_json::from_value(quotas_json.clone()) {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant_id,
                error = %e,
                quotas = %quotas_json,
                "tenants.quotas JSONB does not match TenantQuotaOverrides shape; \
                 falling back to global defaults"
            );
            TenantQuotaOverrides::default()
        }
    };
    Ok(Some((status, quotas)))
}

/// reaper 用: 現存する全テナント id を列挙する (M3d, §8)。
///
/// `tenants` には RLS が無い（0004_rls.sql: tenant_id 列を持たないため対象外）。faas_app は
/// SELECT 権限を持つため GUC 無しで列挙できる。reaper はこの一覧を回し、テナントごとに
/// GUC を設定してから `count_inflight_executions` で DB COUNT（真実）を引き、共有カウンタを
/// 再同期する。`status = 'active'` で停止テナントを除外する（slug 解決と同じ条件）。
pub async fn list_active_tenant_ids(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT id FROM tenants WHERE status = 'active'")
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| r.try_get::<String, _>("id"))
        .collect()
}

/// reaper 用: 当該テナントの in-flight（pending/running）execution 数を数える (M3d, §8)。
///
/// **DB COUNT は in-flight 同時実行数の唯一の真実**であり、Redis カウンタはその速い近似に
/// 過ぎない。reaper はこの値で共有カウンタを上書き再同期し、終端パスでの DECR 取りこぼし／
/// 二重 DECR によるドリフトを定期的に消す。executions は FORCE RLS 下にあるため、呼び出し側は
/// 事前に `set_tenant_guc(tenant)` を同一 tx に設定していること（GUC 未設定は fail-closed ERROR）。
pub async fn count_inflight_executions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM executions \
         WHERE tenant_id = $1 AND status IN ('pending', 'running')",
    )
    .bind(tenant_id)
    .fetch_one(executor)
    .await?;
    row.try_get("n")
}

/// stuck-execution sweeper (M3d, §8): deadline を過ぎても終端化されない pending/running 行を
/// `failed` に CAS finalize し、回収した execution_id 一覧を返す。
///
/// なぜ必要か: invoke ハンドラは「pending 行を commit → JetStream publish」の順で動く。commit 後の
/// publish/ack 失敗で 500 を返すと、ジョブは enqueue されず worker が一切受け取らず、subscriber の
/// finalize も走らないため **pending 行が恒久的に残る**。同様に worker 側の取りこぼし（再配送が
/// max_deliver まで尽きても結果が出ない）でも running/pending が孤立する。reaper の DB COUNT 再同期は
/// この孤立行を「真実」として数えるため回収できず、in-flight スロットが恒久リークする。この sweeper が
/// `created_at < now() - deadline` の非終端行を failed に倒し、呼び出し側が各行を DECR して回収する。
///
/// executions は FORCE RLS 下のため、呼び出し側は事前に同一 tx で `set_tenant_guc(tenant)` 済みのこと。
/// 返す行群が「この pass で実際に pending/running → failed へ遷移させた行」（CAS で 1 度きり）。
///
/// M5 (§15「欠落しない」): 各行は `id` に加え `component_id` と `period_start`（この UPDATE が打った
/// `finished_at` 由来の UTC 日）を返す。呼び出し側（reaper）はこれを使い、sweeper で終端化した実行も
/// `usage_rollups` に `failed` として計上する（invocation +1 / failed +1 / リソース指標 0）。これにより
/// 「worker 落下で sweeper が終端化した実行が集計に乗らない」欠落を塞ぐ。sweep は CAS（既終端は WHERE で
/// 除外）なので再走しても同じ行は返らず、rollup も二重計上にならない。
pub async fn finalize_stuck_executions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    deadline_secs: i64,
) -> Result<Vec<SweptExecution>, sqlx::Error> {
    let rows = sqlx::query(STUCK_EXECUTION_SWEEP_SQL)
        .bind(tenant_id)
        .bind(deadline_secs)
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SweptExecution {
                id: r.try_get::<String, _>("id")?,
                component_id: r.try_get::<String, _>("component_id")?,
                period_start: r.try_get::<NaiveDate, _>("period_start")?,
            })
        })
        .collect()
}

/// `finalize_stuck_executions` が CAS で `failed` 化した 1 行。sweeper 経路の rollup 計上に必要な
/// 最小情報（集計キー）を持つ (M5, §15)。
#[derive(Debug, Clone)]
pub struct SweptExecution {
    pub id: String,
    pub component_id: String,
    /// この UPDATE が打った `finished_at`（DB の `now()`）由来の UTC 日。`usage_rollups.period_start`。
    pub period_start: NaiveDate,
}

/// `finalize_stuck_executions` の SQL。`created_at < now() - deadline` の非終端（pending/running）
/// 行のみを `failed` に遷移させ（CAS: 既終端は WHERE で除外）、遷移した行の集計キーを返す。
/// `$2` は INTERVAL の秒数。`error` には sweeper 由来であることを記録する（監査・調査用）。
/// M5: rollup 計上のため `component_id` と `period_start`（finished_at 由来 UTC 日, 単一時計源）も返す。
const STUCK_EXECUTION_SWEEP_SQL: &str = "UPDATE executions \
     SET status = 'failed', \
         error = '\"execution exceeded delivery deadline without a terminal result (stuck-execution sweeper)\"'::jsonb, \
         finished_at = now() \
     WHERE tenant_id = $1 \
       AND status IN ('pending', 'running') \
       AND created_at < now() - make_interval(secs => $2::double precision) \
     RETURNING id, component_id, (finished_at AT TIME ZONE 'UTC')::date AS period_start";

/// 結果メッセージを CAS 的に反映する (§ result subscriber)。
///
/// 終端状態 (succeeded/failed/timeout) へは「まだ終端でないときだけ」遷移させる。
/// これにより重複配送 / 競合更新が冪等になる。更新された行数を返す。
///
/// M5 (§15): per-execution 計量列（cpu_fuel_used / wall_time_ms / peak_memory_bytes /
/// output_bytes / invocation_count）を **同一 CAS UPDATE の SET 句**に同梱する。これが計量の
/// 冪等アンカー: status と計量が同一行・同一述語（`status NOT IN terminal`）で原子更新されるため、
/// 再配送・worker 多重実行・DLQ 後着は既終端行に当たって rows_affected==0 となり、status だけ no-op で
/// 計量だけ書かれる窓が存在しない（二重計上が構造的に不可能）。`usage` が `None`（旧 worker / DLQ /
/// 未計測）なら計量列は NULL のまま書く（0=計測して 0 と NULL=未計測 を区別）。`invocation_count` は
/// wire に載らないため CP 側で固定（`usage` 有りで 1, 無しで NULL）。`usage` は呼び出し側で
/// `ResourceLimits` 上限に clamp 済み（信頼境界外対策）であることを前提とする。
///
/// 戻り値: CAS が実際に pending/running→終端へ遷移させたとき `Some(period_start)`、既終端 / 行なしで
/// no-op だったとき `None`。`period_start` は `RETURNING (finished_at AT TIME ZONE 'UTC')::date` で
/// この UPDATE が打った `finished_at`（= DB の `now()`）から導出した **UTC 日**であり、`usage_rollups`
/// の `period_start` に使う。CP 側の `Utc::now()` ではなく **同一 finalize の単一時計源**にすることで、
/// per-execution の `finished_at` と集計の日付帰属が UTC 日境界をまたぐ瞬間にもズレない（§15 M5）。
pub async fn finalize_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
    status: ExecutionStatus,
    output: Option<&Value>,
    error: Option<&Value>,
    usage: Option<&UsageMetrics>,
) -> Result<Option<NaiveDate>, sqlx::Error> {
    debug_assert!(status.is_terminal());

    // 計量列は nullable（BIGINT/INTEGER）。usage None のときは全て NULL バインドする。
    // u64 → i64 の格納は clamp 済み前提だが、念のため飽和でオーバーフローを避ける。
    let cpu_fuel_used = usage.map(|u| saturating_i64(u.cpu_fuel_used));
    let wall_time_ms = usage.map(|u| saturating_i64(u.wall_time_ms));
    let peak_memory_bytes = usage.map(|u| saturating_i64(u.peak_memory_bytes));
    let output_bytes = usage.map(|u| saturating_i64(u.output_bytes));
    let invocation_count: Option<i32> = usage.map(|_| 1);

    // RETURNING を `fetch_optional` で受ける: CAS が当たれば 1 行（period_start: UTC 日）、
    // 既終端で当たらなければ 0 行（None）。rows_affected を数える代わりに、遷移有無と
    // 集計用日付を **1 ステートメント**で同時に得る（二重計上の冪等アンカーは不変）。
    let period_start: Option<NaiveDate> = sqlx::query_scalar(FINALIZE_EXECUTION_SQL)
        .bind(tenant_id)
        .bind(execution_id)
        .bind(status.as_str())
        .bind(output)
        .bind(error)
        .bind(cpu_fuel_used)
        .bind(wall_time_ms)
        .bind(peak_memory_bytes)
        .bind(output_bytes)
        .bind(invocation_count)
        .fetch_optional(executor)
        .await?;

    Ok(period_start)
}

/// `u64` を `i64`（Postgres BIGINT）へ飽和変換する。計量は呼び出し側で `ResourceLimits` 上限に
/// clamp 済み（信頼境界外対策）だが、二重防御として `i64::MAX` で頭打ちにし、桁あふれによる
/// 負値混入や格納失敗を防ぐ（純関数・DB 非依存でテスト可能）。
fn saturating_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// finalize と同一 tx で打つ `usage_rollups` の増分 UPSERT (M5, §15)。
///
/// 集計粒度はテナント×UTC日（`period_start`）×component。`ON CONFLICT (tenant_id, period_start,
/// component_id)` の複合 PK を競合ターゲットにして、SUM 列（invocation_count / cpu_fuel_used /
/// wall_time_ms / output_bytes / 各 count）は加算、`peak_memory_bytes_max` は `GREATEST` で MAX 更新する。
///
/// **冪等の要**: 呼び出し側（`commit_finalize_and_release`）は `finalize_execution` が実際に
/// pending/running→終端へ遷移させた（rows_affected==1）ときだけ本関数を呼ぶ。再配送 / 既終端 /
/// sweeper 先着（updated==0）では一切呼ばない＝二重計上しない。さらに finalize の UPDATE と同一 tx 内・
/// commit 前に発行されるため、『executions は終端化したが rollup は未反映』『rollup は加算したが finalize は
/// ロールバック』という乖離は起きない（all-or-nothing）。
///
/// `invocation_count` は全終端で +1 し、`status` から `succeeded_count`/`failed_count`/`timeout_count`
/// のどれか 1 つを +1 する。`usage` のリソース指標（cpu/wall/peak/output）は計測済みなら加算、DLQ/timeout の
/// `UsageMetrics::default()`（= 全 0）なら 0 加算となる（invocation は全終端で計上、リソースは計測済みのみ
/// という半端行になりうる; 解釈は API ドキュメントで明示する, §15 設計）。
pub async fn upsert_usage_rollup(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    period_start: NaiveDate,
    status: ExecutionStatus,
    usage: &UsageMetrics,
) -> Result<(), sqlx::Error> {
    debug_assert!(status.is_terminal());

    // status から終端カウンタの 0/1 を導出する（DLQ は常に Failed に倒される）。
    let succeeded_count: i64 = (status == ExecutionStatus::Succeeded) as i64;
    let failed_count: i64 = (status == ExecutionStatus::Failed) as i64;
    let timeout_count: i64 = (status == ExecutionStatus::Timeout) as i64;

    sqlx::query(UPSERT_USAGE_ROLLUP_SQL)
        .bind(tenant_id)
        .bind(period_start)
        .bind(component_id)
        .bind(saturating_i64(usage.cpu_fuel_used))
        .bind(saturating_i64(usage.wall_time_ms))
        .bind(saturating_i64(usage.peak_memory_bytes))
        .bind(saturating_i64(usage.output_bytes))
        .bind(succeeded_count)
        .bind(failed_count)
        .bind(timeout_count)
        .execute(executor)
        .await?;

    Ok(())
}

/// `upsert_usage_rollup` が発行する増分 UPSERT SQL。複合 PK を競合ターゲットに、SUM 列は加算、
/// `peak_memory_bytes_max` は `GREATEST`（MAX セマンティクス）で更新する。定数に切り出して
/// DB 非依存のユニットテストで集計セマンティクスを静的検査できるようにする。
const UPSERT_USAGE_ROLLUP_SQL: &str = "INSERT INTO usage_rollups \
     (tenant_id, period_start, component_id, invocation_count, cpu_fuel_used, wall_time_ms, \
      peak_memory_bytes_max, output_bytes, succeeded_count, failed_count, timeout_count) \
     VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (tenant_id, period_start, component_id) DO UPDATE SET \
       invocation_count = usage_rollups.invocation_count + 1, \
       cpu_fuel_used = usage_rollups.cpu_fuel_used + EXCLUDED.cpu_fuel_used, \
       wall_time_ms = usage_rollups.wall_time_ms + EXCLUDED.wall_time_ms, \
       peak_memory_bytes_max = GREATEST(usage_rollups.peak_memory_bytes_max, EXCLUDED.peak_memory_bytes_max), \
       output_bytes = usage_rollups.output_bytes + EXCLUDED.output_bytes, \
       succeeded_count = usage_rollups.succeeded_count + EXCLUDED.succeeded_count, \
       failed_count = usage_rollups.failed_count + EXCLUDED.failed_count, \
       timeout_count = usage_rollups.timeout_count + EXCLUDED.timeout_count, \
       updated_at = now()";

/// `GET /usage` の応答用 component 別集計 1 行 (M5, §15 / §6.0)。
///
/// `usage_rollups`（テナント×UTC日×component 粒度）を期間で `GROUP BY component_id` した結果。
/// SUM 列は和、`peak_memory_bytes_max` は期間内の最大（行レベルの MAX セマンティクスをさらに集約）。
/// `invocation_count` は全終端で +1 されているため、計測済みリソース指標を持たない DLQ/timeout も
/// 含む（解釈は API ドキュメント参照: invocation は全終端、リソース指標は計測済みのみ）。
#[derive(Debug, Clone)]
pub struct UsageRollupRow {
    pub component_id: String,
    pub invocation_count: i64,
    pub cpu_fuel_used: i64,
    pub wall_time_ms: i64,
    pub peak_memory_bytes_max: i64,
    pub output_bytes: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timeout_count: i64,
}

/// `GET /usage` 用に `usage_rollups` を期間集計する (M5, §15 / §6.0, read スコープ)。
///
/// `period_start`（UTC 日境界）が `[from, to]`（両端含む）にある行を component 別に集約する。
/// `tenant_id` は RLS の GUC（`set_tenant_guc`）と二重防御で WHERE にもバインドする（既存 db
/// クエリ規約。文字列結合はしない: injection 防止）。`component_id` が `Some` なら単一 component に
/// 絞り込む（`$4::text IS NULL OR ...` で NULL なら全件）。totals はハンドラ側で本行を畳んで算出する。
pub async fn get_usage_rollups(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    from: NaiveDate,
    to: NaiveDate,
    component_id: Option<&str>,
) -> Result<Vec<UsageRollupRow>, sqlx::Error> {
    let rows = sqlx::query(
        // SUM(bigint) は Postgres では NUMERIC を返すため、各集計を ::bigint へ明示キャストして
        // Rust 側の i64 デコード（UsageRollupRow）と型を一致させる（MAX は元の bigint を保つ）。
        // 行数 × 各列とも実運用域では i64 に収まる（saturating_i64 で書込時に頭打ち済み）。
        "SELECT component_id, \
                SUM(invocation_count)::bigint AS invocation_count, \
                SUM(cpu_fuel_used)::bigint AS cpu_fuel_used, \
                SUM(wall_time_ms)::bigint AS wall_time_ms, \
                MAX(peak_memory_bytes_max) AS peak_memory_bytes_max, \
                SUM(output_bytes)::bigint AS output_bytes, \
                SUM(succeeded_count)::bigint AS succeeded_count, \
                SUM(failed_count)::bigint AS failed_count, \
                SUM(timeout_count)::bigint AS timeout_count \
         FROM usage_rollups \
         WHERE tenant_id = $1 AND period_start >= $2 AND period_start <= $3 \
           AND ($4::text IS NULL OR component_id = $4) \
         GROUP BY component_id \
         ORDER BY component_id",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(UsageRollupRow {
                component_id: r.try_get("component_id")?,
                invocation_count: r.try_get("invocation_count")?,
                cpu_fuel_used: r.try_get("cpu_fuel_used")?,
                wall_time_ms: r.try_get("wall_time_ms")?,
                peak_memory_bytes_max: r.try_get("peak_memory_bytes_max")?,
                output_bytes: r.try_get("output_bytes")?,
                succeeded_count: r.try_get("succeeded_count")?,
                failed_count: r.try_get("failed_count")?,
                timeout_count: r.try_get("timeout_count")?,
            })
        })
        .collect()
}

/// `finalize_execution` が発行する SQL。終端遷移は **CAS**（compare-and-set）で行う:
/// `WHERE ... AND status NOT IN (terminal)` により、既に終端の行には 0 行しか当たらない
/// （= 重複 result の再配送や worker 多重実行があっても終端状態を上書きしない, §6.6）。
/// 定数に切り出して DB 非依存のユニットテストで CAS ガードの存在を検査できるようにする。
/// M5 (§15): per-execution 計量列（$6..$10）を同一 SET 句に同梱する。CAS ガード
/// （`WHERE ... AND status NOT IN (terminal)`）は不変。これにより重複 result は 0 行更新となり、
/// 計量列も同時に no-op になる（status と計量が同一行・同一述語で原子更新されるため、status だけ no-op で
/// 計量だけ書かれる窓が存在しない＝二重計上が構造的に不可能）。
const FINALIZE_EXECUTION_SQL: &str = "UPDATE executions \
     SET status = $3, output = $4, error = $5, finished_at = now(), \
         cpu_fuel_used = $6, wall_time_ms = $7, peak_memory_bytes = $8, \
         output_bytes = $9, invocation_count = $10 \
     WHERE tenant_id = $1 AND id = $2 \
       AND status NOT IN ('succeeded', 'failed', 'timeout') \
     RETURNING (finished_at AT TIME ZONE 'UTC')::date AS period_start";

// ---------------------------------------------------------------------------
// 認証・認可: users / api_tokens (§3.3 / §6.0)
// ---------------------------------------------------------------------------

/// `api_tokens` の照合結果行（認証ホットパス）。
///
/// 失効/期限の判定は `into_principal`（純関数化のため `is_token_valid` に委譲）で行う。
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub token_id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// 紐づくユーザのロール。サービストークン（user_id NULL）は `None`。
    pub user_role: Option<String>,
}

/// トークンが現時点で有効か（失効しておらず、かつ未期限切れ）。
///
/// 純関数（DB 不要）でテスト可能にしておく。`now` を引数に取り、時計依存を排除する。
pub fn is_token_valid(
    revoked_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    revoked_at.is_none() && expires_at > now
}

/// 文字列スコープ集合を `Scope` ベクタへパースする。未知の文字列は破棄する。
pub fn parse_scopes(raw: &[String]) -> Vec<Scope> {
    raw.iter()
        .filter_map(|s| match s.as_str() {
            "read" => Some(Scope::Read),
            "invoke" => Some(Scope::Invoke),
            "deploy" => Some(Scope::Deploy),
            "admin" => Some(Scope::Admin),
            _ => None,
        })
        .collect()
}

/// 文字列ロールを `Role` へパースする。未知/欠損は `None`。
pub fn parse_role(raw: Option<&str>) -> Option<Role> {
    match raw {
        Some("member") => Some(Role::Member),
        Some("admin") => Some(Role::Admin),
        _ => None,
    }
}

impl TokenRow {
    /// 有効なトークンなら `Principal` を構築する。失効/期限切れは `None`。
    ///
    /// サービストークン（user_id NULL）はユーザロールが無いため `Role::Admin`
    /// 扱いとする（ロール上限ではなく付与済み scopes でのみ権限が制約される）。
    pub fn into_principal(self) -> Option<Principal> {
        self.into_principal_at(Utc::now())
    }

    /// `now` を明示する版（テスト用）。
    pub fn into_principal_at(self, now: DateTime<Utc>) -> Option<Principal> {
        if !is_token_valid(self.revoked_at, self.expires_at, now) {
            return None;
        }
        let role = match self.user_role.as_deref() {
            None => Role::Admin, // サービストークン（ユーザ無し）。
            other => parse_role(other)?,
        };
        Some(Principal {
            tenant_id: self.tenant_id,
            user_id: self.user_id,
            token_id: self.token_id,
            scopes: parse_scopes(&self.scopes),
            role,
        })
    }
}

/// token_hash で API トークンを照合する（認証ホットパス, §3.3 / §15 M3b）。
///
/// 平文比較ではなく UNIQUE インデックス照合に委ねる（非定数時間比較を排除）。
/// 失効/期限切れの判定は呼び出し側（`TokenRow::into_principal`）で行う。
///
/// M3b: これは**テナントコンテキスト確立前**に走る（戻り値の tenant_id がこの後の
/// リクエストで GUC を設定する）。よって FORCE RLS 下では GUC 未設定で api_tokens を
/// 読めない。SECURITY DEFINER 関数 `auth_lookup_token_by_hash`（所有者権限で実行され
/// RLS 対象外）を呼ぶことで、GUC 無しの認証前参照を成立させる（migrations/0004_rls.sql）。
pub async fn find_token_by_hash(
    executor: impl sqlx::PgExecutor<'_>,
    token_hash: &str,
) -> Result<Option<TokenRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, tenant_id, user_id, scopes, expires_at, revoked_at, user_role \
         FROM auth_lookup_token_by_hash($1)",
    )
    .bind(token_hash)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(TokenRow {
            token_id: r.try_get("id")?,
            tenant_id: r.try_get("tenant_id")?,
            user_id: r.try_get("user_id")?,
            scopes: r.try_get("scopes")?,
            expires_at: r.try_get("expires_at")?,
            revoked_at: r.try_get("revoked_at")?,
            user_role: r.try_get("user_role")?,
        })
    })
    .transpose()
}

/// テナントを作成する（bootstrap: POST /admin/tenants）。slug は UNIQUE。
/// テナントと最初の admin ユーザを**1 トランザクション**で作成する（§9 bootstrap）。
///
/// `POST /admin/tenants` の chicken-and-egg を解消する: 通常の `POST
/// /tenants/{id}/users` は Admin トークンを要するが、最初のユーザはまだトークンが
/// 無いため作れない。bootstrap トークンで保護されたこの経路だけが、テナントと
/// 最初の admin ユーザを同時に作る。失敗時は両方ロールバックされる。
///
/// M3b: マルチステートメント（tenants + users INSERT を 1 単位で実行）のため
/// `impl PgExecutor` ではなく `&mut PgConnection` を受け、tx は呼び出し側が所有する
/// （内部 begin/commit は撤去）。呼び出し側は **同一 tx 上で先に**
/// `set_tenant_guc(&mut *tx, tenant_id)` を呼ぶこと: users は FORCE RLS 下で
/// WITH CHECK が GUC と一致する必要がある（tenants には RLS は無い）。
#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_tenant(
    conn: &mut sqlx::PgConnection,
    tenant_id: &str,
    slug: &str,
    name: &str,
    admin_user_id: &str,
    admin_email: &str,
    admin_password_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO tenants (id, slug, name, status) VALUES ($1, $2, $3, 'active')")
        .bind(tenant_id)
        .bind(slug)
        .bind(name)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'admin')",
    )
    .bind(admin_user_id)
    .bind(tenant_id)
    .bind(admin_email)
    .bind(admin_password_hash)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// ユーザを作成する（POST /tenants/{id}/users）。`tenant_id` でスコープする。
pub async fn create_user(
    executor: impl sqlx::PgExecutor<'_>,
    user_id: &str,
    tenant_id: &str,
    email: &str,
    password_hash: &str,
    role: Role,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(email)
    .bind(password_hash)
    .bind(role.as_str())
    .execute(executor)
    .await?;
    Ok(())
}

/// login / トークン発行で参照するユーザ行。
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub password_hash: String,
    pub role: String,
}

/// (tenant_id, email) でユーザを解決する（soft-delete 済みは除外）。
///
/// M3b: login の認証前参照（Principal 確立前。slug 解決の後だが GUC はまだ無い）。
/// users は FORCE RLS のため GUC 無しでは読めないので、SECURITY DEFINER 関数
/// `auth_lookup_user_by_email` を呼ぶ（migrations/0004_rls.sql）。
pub async fn find_user_by_email(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    email: &str,
) -> Result<Option<UserRow>, sqlx::Error> {
    let row = sqlx::query("SELECT id, password_hash, role FROM auth_lookup_user_by_email($1, $2)")
        .bind(tenant_id)
        .bind(email)
        .fetch_optional(executor)
        .await?;

    row.map(|r| {
        Ok(UserRow {
            id: r.try_get("id")?,
            password_hash: r.try_get("password_hash")?,
            role: r.try_get("role")?,
        })
    })
    .transpose()
}

/// テナント内ユーザを id で解決し role を返す（トークン発行の対象ユーザ確認用）。
///
/// テナント外/不在は `None`（IDOR: 存在秘匿のため呼び出し側で 404 にする）。
pub async fn find_user_role(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT role FROM users \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| r.try_get::<String, _>("role")).transpose()
}

/// slug からテナントを解決する（login のテナント解決, §3.3）。active のみ。
///
/// M3b: login の最初の認証前参照（GUC 無し）。tenants 自体には RLS は無いが、規約として
/// 3 つの認証前参照を SECURITY DEFINER 関数経由に統一する。`auth_lookup_tenant_id_by_slug`
/// は `RETURNS TABLE(id text)` のため、未一致は 0 行 → `fetch_optional` で `None`（スカラ
/// NULL 行の誤判定を避ける, migrations/0004_rls.sql）。
pub async fn find_tenant_id_by_slug(
    executor: impl sqlx::PgExecutor<'_>,
    slug: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT id FROM auth_lookup_tenant_id_by_slug($1)")
        .bind(slug)
        .fetch_optional(executor)
        .await?;

    row.map(|r| r.try_get::<String, _>("id")).transpose()
}

/// API トークン行を作成する。`scopes` は文字列配列で渡す（CHECK 制約に合致させる）。
#[allow(clippy::too_many_arguments)]
pub async fn create_token(
    executor: impl sqlx::PgExecutor<'_>,
    token_id: &str,
    tenant_id: &str,
    user_id: Option<&str>,
    token_hash: &str,
    scopes: &[String],
    name: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_tokens \
         (id, tenant_id, user_id, token_hash, scopes, name, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(token_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .bind(scopes)
    .bind(name)
    .bind(expires_at)
    .execute(executor)
    .await?;
    Ok(())
}

/// トークンを失効させる（DELETE /tokens/{id}）。
///
/// IDOR 対策: 必ず呼び出し主体の `tenant_id` でスコープする。更新 0 行なら
/// テナント外/不在（呼び出し側は 404 にする＝存在秘匿）。
pub async fn revoke_token(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    token_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() \
         WHERE tenant_id = $1 AND id = $2 AND revoked_at IS NULL",
    )
    .bind(tenant_id)
    .bind(token_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// 指定 token_id がテナント内に存在するか（revoke の 404/204 判定補助）。
pub async fn token_exists(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    token_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM api_tokens WHERE tenant_id = $1 AND id = $2) AS present",
    )
    .bind(tenant_id)
    .bind(token_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

// ---------------------------------------------------------------------------
// 追記専用 audit ログ (§3.2 / §3.7, M3c)
// ---------------------------------------------------------------------------

/// audit_logs に 1 行 INSERT する（追記専用; §3.2）。
///
/// FORCE RLS + tenant_isolation 下で動くため、**呼び出し側は同一 tx 上で先に**
/// `set_tenant_guc(&mut *tx, tenant_id)` を呼ぶこと（WITH CHECK が GUC と一致しないと失敗する）。
/// よって `tenant_id` は GUC と同じ値（subscriber では subject 由来テナント＝唯一権威ある値）を渡す。
/// `detail` には理由・id のみ載せる（生トークン・秘密は記録しない, §3.7）。
/// 追記専用のため UPDATE/DELETE は提供しない（migrations/0005 が faas_app に付与していない）。
pub async fn insert_audit_log(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    actor: Option<&str>,
    action: &str,
    target: Option<&str>,
    detail: Option<&Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (tenant_id, actor, action, target, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(executor)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Cron ジョブ (M6b, §11 / §15)
// ---------------------------------------------------------------------------

/// `cron_jobs` の 1 行（CRUD API 応答 + スケジューラの fire に必要な範囲）。
#[derive(Debug, Clone)]
pub struct CronJobRow {
    pub id: String,
    pub component_id: String,
    pub schedule: String,
    pub input: Value,
    pub enabled: bool,
    pub next_fire_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl CronJobRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            component_id: row.try_get("component_id")?,
            schedule: row.try_get("schedule")?,
            input: row.try_get("input")?,
            enabled: row.try_get("enabled")?,
            next_fire_at: row.try_get("next_fire_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// Cron ジョブを 1 件 INSERT する (M6b, `POST /cron-jobs`)。
///
/// FORCE RLS + tenant_isolation 下で動くため、**呼び出し側は同一 tx 上で先に**
/// `set_tenant_guc(&mut *tx, tenant_id)` を呼ぶこと（WITH CHECK が GUC と一致しないと失敗する）。
/// `component_id` は呼び出し側が存在検証済み（FK でも担保）、`schedule` は cron 式パース済み、
/// `next_fire_at` は初回発火時刻（`cron::first_fire_after`）を渡す。`input` は fire 時に Component へ
/// 渡す入力（未指定は `null`）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_cron_job(
    executor: impl sqlx::PgExecutor<'_>,
    id: &str,
    tenant_id: &str,
    component_id: &str,
    schedule: &str,
    input: &Value,
    next_fire_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO cron_jobs \
         (id, tenant_id, component_id, schedule, input, next_fire_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(component_id)
    .bind(schedule)
    .bind(input)
    .bind(next_fire_at)
    .execute(executor)
    .await?;
    Ok(())
}

/// テナントの Cron ジョブ一覧 (M6b, `GET /cron-jobs`)。新しい順。
///
/// FORCE RLS 下のため呼び出し側は同一 tx で `set_tenant_guc(tenant)` 済みのこと。
pub async fn list_cron_jobs(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<Vec<CronJobRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, component_id, schedule, input, enabled, next_fire_at, created_at \
         FROM cron_jobs WHERE tenant_id = $1 ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(executor)
    .await?;
    rows.iter().map(CronJobRow::from_row).collect()
}

/// Cron ジョブを 1 件物理削除する (M6b, `DELETE /cron-jobs/{id}`)。更新行数を返す。
///
/// FORCE RLS 下のため呼び出し側は同一 tx で `set_tenant_guc(tenant)` 済みのこと（GUC=テナントに
/// 一致する行しか消せない＝cross-tenant 削除は構造的に不可）。soft-delete は持たない
/// （登録の取り消しは単純に行を消す。実行済み execution は executions 側に独立して残る）。
pub async fn delete_cron_job(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM cron_jobs WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected())
}

/// 全テナント横断で「いま due な Cron ジョブ」の (tenant_id, job_id) を引く (M6b スケジューラ)。
///
/// `cron_due_tenant_jobs()`（SECURITY DEFINER, migrations/0008_m6.sql）を呼ぶ。所有者権限で
/// 実行されるため faas_app の FORCE RLS の対象外で、**GUC 未設定のまま**全テナントの due 行を
/// 引ける（reaper の `list_active_tenant_ids` と同型の認証前/巡回参照）。返るのは (tenant, job_id)
/// だけで、fire 本処理は呼び出し側が各テナントごとに `set_tenant_guc` した tx で RLS 下で行う。
pub async fn cron_due_tenant_jobs(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows = sqlx::query("SELECT tenant_id, job_id FROM cron_due_tenant_jobs()")
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>("tenant_id")?,
                r.try_get::<String, _>("job_id")?,
            ))
        })
        .collect()
}

/// due な Cron ジョブ 1 行を **`FOR UPDATE SKIP LOCKED`** で掴む (M6b single-flight)。
///
/// `cron_due_tenant_jobs()` で対象を引いたあと、各テナントの tx（`set_tenant_guc` 済み）で当該
/// job_id の行を行ロックする。**最初にロックを掴んだ CP だけ**が `Some(row)` を得て fire し、他 CP は
/// `SKIP LOCKED` で `None`（= 先取りされたので skip）になる。`enabled AND next_fire_at <= now()` を
/// 再判定するのは、ロック獲得までの間に他 CP が next_fire_at を前進させて due を消した競合に対応する
/// ため（掴んだ瞬間に「まだ due か」を権威的に確認する）。FORCE RLS 下のため呼び出し側は同一 tx で
/// `set_tenant_guc(tenant)` 済みのこと。
pub async fn lock_due_cron_job(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    job_id: &str,
) -> Result<Option<CronJobRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, component_id, schedule, input, enabled, next_fire_at, created_at \
         FROM cron_jobs \
         WHERE tenant_id = $1 AND id = $2 AND enabled AND next_fire_at <= now() \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(tenant_id)
    .bind(job_id)
    .fetch_optional(executor)
    .await?;
    row.as_ref().map(CronJobRow::from_row).transpose()
}

/// 掴んだ Cron ジョブの `next_fire_at` を次回 occurrence へ前進させる (M6b)。
///
/// `lock_due_cron_job` で行ロックを保持している同一 tx 内で呼ぶ。`last_fired_slot` も今回 fire の
/// scheduled_slot で更新する（冪等補助・観測用）。next_fire_at を前進させてから enqueue・commit する
/// ことで、ロックを保持している間に同一行の重複 due を消し、次の poll では due に当たらなくする。
pub async fn advance_cron_next_fire(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    id: &str,
    next_fire_at: DateTime<Utc>,
    fired_slot: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE cron_jobs \
         SET next_fire_at = $3, last_fired_slot = $4, updated_at = now() \
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(next_fire_at)
    .bind(fired_slot)
    .execute(executor)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// トリガー (M6c, §11 / §15)
// ---------------------------------------------------------------------------

/// `triggers` の 1 行（CRUD API 応答 + chain 解決 / event ルーティングに必要な範囲）。
#[derive(Debug, Clone)]
pub struct TriggerRow {
    pub id: String,
    pub component_id: String,
    pub trigger_type: String,
    pub match_config: Value,
    pub input_mapping: Option<Value>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl TriggerRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            component_id: row.try_get("component_id")?,
            trigger_type: row.try_get("trigger_type")?,
            match_config: row.try_get("match_config")?,
            input_mapping: row.try_get("input_mapping")?,
            enabled: row.try_get("enabled")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// トリガーを 1 件 INSERT する (M6c, `POST /triggers`)。
///
/// FORCE RLS + tenant_isolation 下で動くため、呼び出し側は同一 tx 上で先に
/// `set_tenant_guc(&mut *tx, tenant_id)` を呼ぶこと（WITH CHECK が GUC と一致しないと失敗する）。
/// `component_id`（downstream 起動対象）は呼び出し側が存在検証済み（FK でも担保）、`trigger_type` /
/// `match_config` / `input_mapping` は登録時に検証済み（不正は 422）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_trigger(
    executor: impl sqlx::PgExecutor<'_>,
    id: &str,
    tenant_id: &str,
    component_id: &str,
    trigger_type: &str,
    match_config: &Value,
    input_mapping: Option<&Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO triggers \
         (id, tenant_id, component_id, trigger_type, match_config, input_mapping) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(component_id)
    .bind(trigger_type)
    .bind(match_config)
    .bind(input_mapping)
    .execute(executor)
    .await?;
    Ok(())
}

/// テナントのトリガー一覧 (M6c, `GET /triggers`)。新しい順。
///
/// FORCE RLS 下のため呼び出し側は同一 tx で `set_tenant_guc(tenant)` 済みのこと。
pub async fn list_triggers(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<Vec<TriggerRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, component_id, trigger_type, match_config, input_mapping, enabled, created_at \
         FROM triggers WHERE tenant_id = $1 ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(executor)
    .await?;
    rows.iter().map(TriggerRow::from_row).collect()
}

/// トリガーを 1 件物理削除する (M6c, `DELETE /triggers/{id}`)。更新行数を返す。
///
/// FORCE RLS 下のため呼び出し側は同一 tx で `set_tenant_guc(tenant)` 済みのこと（GUC=テナントに
/// 一致する行しか消せない＝cross-tenant 削除は構造的に不可）。
pub async fn delete_trigger(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM triggers WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected())
}

/// テナント内の **enabled な指定タイプのトリガー** を引く (M6c)。
///
/// object_storage event のルーティング（`trigger_type='object_storage'`）と chain 解決
/// （`trigger_type='chain'`）の双方で使う。`match_config` からの式 index は避け、テナント内の
/// 該当タイプ trigger を引いて **アプリ側で照合**する（bucket_prefix / source_component_id）。
/// 件数はテナントあたりのトリガー数に比例（実運用で十分小さい前提）。FORCE RLS 下のため呼び出し側は
/// 同一 tx で `set_tenant_guc(tenant)` 済みのこと（cross-tenant trigger は構造的に引けない）。
pub async fn list_enabled_triggers_by_type(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    trigger_type: &str,
) -> Result<Vec<TriggerRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, component_id, trigger_type, match_config, input_mapping, enabled, created_at \
         FROM triggers \
         WHERE tenant_id = $1 AND enabled AND trigger_type = $2",
    )
    .bind(tenant_id)
    .bind(trigger_type)
    .fetch_all(executor)
    .await?;
    rows.iter().map(TriggerRow::from_row).collect()
}

/// 配送台帳 `trigger_deliveries` へ 1 件 INSERT する (M6c, 配送冪等の権威)。
///
/// 戻り値 `true` = **新規配送**（INSERT 成功）、`false` = **既配送**（PK 23505 = 同一
/// (tenant, trigger, event_dedup_id) が既に在る → 二重起動を skip すべき）。これにより
/// object-storage イベントの再送 / chain 上流成功の再 finalize でも downstream を 1 度だけ起動する
/// （`executions(tenant_id, idempotency_key)` UNIQUE と併せた二重防御, §6.6）。
///
/// 台帳は append-only（migrations/0008_m6.sql で UPDATE/DELETE 非付与）なので、INSERT は ON CONFLICT
/// を使わず素の INSERT で 23505 を捕捉する（配送の権威を改竄不能にする）。FORCE RLS 下のため呼び出し側は
/// 同一 tx で `set_tenant_guc(tenant)` 済みのこと。
pub async fn record_trigger_delivery(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    trigger_id: &str,
    event_dedup_id: &str,
    execution_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "INSERT INTO trigger_deliveries \
         (tenant_id, trigger_id, event_dedup_id, execution_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id)
    .bind(trigger_id)
    .bind(event_dedup_id)
    .bind(execution_id)
    .execute(executor)
    .await;
    match r {
        Ok(_) => Ok(true),
        Err(e) => {
            // PK 23505 = 既配送。二重起動を skip するシグナルとして false を返す（エラーにしない）。
            if matches!(&e, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505"))
            {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        saturating_i64, TenantQuotaOverrides, FINALIZE_EXECUTION_SQL, PROMOTE_ACTIVE_VERSION_SQL,
        RESOLVE_ROUTING_SQL, ROLLBACK_ACTIVE_VERSION_SQL, SET_TENANT_GUC_SQL,
        STUCK_EXECUTION_SWEEP_SQL, SWITCH_ACTIVE_VERSION_SQL, UPSERT_USAGE_ROLLUP_SQL,
    };

    // ---- M4d クォータ上書き JSONB のパース不変条件（DB 非依存）-----------------

    /// 空 JSONB（既定 '{}'）はすべて None（= グローバル既定を継承）。
    #[test]
    fn tenant_quota_overrides_empty_object_is_all_none() {
        let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(v.invoke_rate_per_sec.is_none());
        assert!(v.invoke_burst.is_none());
        assert!(v.max_concurrent_executions.is_none());
    }

    /// 既知キーは読み取り、未知キーは無視する（前方互換）。
    #[test]
    fn tenant_quota_overrides_reads_known_ignores_unknown() {
        let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
            "invoke_rate_per_sec": 100,
            "invoke_burst": 1000,
            "max_concurrent_executions": 50,
            "future_field_we_dont_know": "ignored",
        }))
        .unwrap();
        assert_eq!(v.invoke_rate_per_sec, Some(100));
        assert_eq!(v.invoke_burst, Some(1000));
        assert_eq!(v.max_concurrent_executions, Some(50));
    }

    /// 部分的な上書き: 設定されていないキーは継承（None）になる。
    #[test]
    fn tenant_quota_overrides_partial_override() {
        let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
            "invoke_rate_per_sec": 200,
        }))
        .unwrap();
        assert_eq!(v.invoke_rate_per_sec, Some(200));
        assert!(v.invoke_burst.is_none());
        assert!(v.max_concurrent_executions.is_none());
    }

    /// null 値は None として扱う（明示的「継承」）。
    #[test]
    fn tenant_quota_overrides_null_means_inherit() {
        let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
            "invoke_rate_per_sec": null,
            "max_concurrent_executions": 10,
        }))
        .unwrap();
        assert!(v.invoke_rate_per_sec.is_none());
        assert_eq!(v.max_concurrent_executions, Some(10));
    }

    /// finalize_execution は **CAS** で終端遷移する: `status NOT IN (terminal)` ガードにより
    /// 既終端の行には 0 行しか当たらない（= 重複 result / worker 多重実行があっても終端状態を
    /// 上書きしない, §6.6）。DB 非依存で SQL テキストのガードを静的検査する（退行ガード）。
    #[test]
    fn finalize_execution_sql_is_cas_guarded() {
        // 終端状態の上書きを防ぐ CAS ガードを必ず持つ。
        assert!(
            FINALIZE_EXECUTION_SQL.contains("status NOT IN ('succeeded', 'failed', 'timeout')"),
            "finalize must be a CAS (no-op when row already terminal)"
        );
        // tenant_id / id はバインドパラメータ（テナント境界 + injection 防止）。
        assert!(FINALIZE_EXECUTION_SQL.contains("tenant_id = $1"));
        assert!(FINALIZE_EXECUTION_SQL.contains("id = $2"));
        // 単一 UPDATE（DELETE を伴わない）。
        let upper = FINALIZE_EXECUTION_SQL.to_ascii_uppercase();
        assert!(upper.starts_with("UPDATE EXECUTIONS"));
        assert!(!upper.contains("DELETE"));
        // M5 (§15): 計量列を同一 SET 句に同梱する（status と計量が同一行・同一述語で原子更新される）。
        for col in [
            "cpu_fuel_used = $6",
            "wall_time_ms = $7",
            "peak_memory_bytes = $8",
            "output_bytes = $9",
            "invocation_count = $10",
        ] {
            assert!(
                FINALIZE_EXECUTION_SQL.contains(col),
                "finalize must carry metering column {col} in the same CAS UPDATE"
            );
        }
    }

    /// M5 (§15): rollup の増分 UPSERT は (1) 複合 PK を競合ターゲットにし、(2) SUM 列を加算、
    /// (3) `peak_memory_bytes_max` は `GREATEST`（MAX セマンティクス）で更新する。DB 非依存で
    /// SQL テキストの集計セマンティクスを静的検査する（退行ガード）。
    #[test]
    fn upsert_usage_rollup_sql_has_sum_and_max_semantics() {
        // 複合 PK を競合ターゲットにする。
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("ON CONFLICT (tenant_id, period_start, component_id) DO UPDATE"));
        // SUM 列は既存値に加算する。
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("invocation_count = usage_rollups.invocation_count + 1"));
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("cpu_fuel_used = usage_rollups.cpu_fuel_used + EXCLUDED.cpu_fuel_used"));
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("wall_time_ms = usage_rollups.wall_time_ms + EXCLUDED.wall_time_ms"));
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("output_bytes = usage_rollups.output_bytes + EXCLUDED.output_bytes"));
        // peak は MAX（GREATEST）で更新する（SUM ではない）。
        assert!(UPSERT_USAGE_ROLLUP_SQL.contains(
            "peak_memory_bytes_max = GREATEST(usage_rollups.peak_memory_bytes_max, EXCLUDED.peak_memory_bytes_max)"
        ));
        // 終端カウンタも加算する。
        assert!(UPSERT_USAGE_ROLLUP_SQL.contains(
            "succeeded_count = usage_rollups.succeeded_count + EXCLUDED.succeeded_count"
        ));
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("failed_count = usage_rollups.failed_count + EXCLUDED.failed_count"));
        assert!(UPSERT_USAGE_ROLLUP_SQL
            .contains("timeout_count = usage_rollups.timeout_count + EXCLUDED.timeout_count"));
        // テナント境界はバインドパラメータ（$1）。DELETE は伴わない。
        assert!(UPSERT_USAGE_ROLLUP_SQL.contains("tenant_id"));
        assert!(!UPSERT_USAGE_ROLLUP_SQL
            .to_ascii_uppercase()
            .contains("DELETE"));
    }

    /// `saturating_i64` は `i64::MAX` で頭打ちにし、負値混入や格納失敗を防ぐ（二重防御）。
    #[test]
    fn saturating_i64_clamps_at_i64_max() {
        assert_eq!(saturating_i64(0), 0);
        assert_eq!(saturating_i64(123), 123);
        assert_eq!(saturating_i64(i64::MAX as u64), i64::MAX);
        assert_eq!(saturating_i64(u64::MAX), i64::MAX);
    }

    /// stuck-execution sweeper の SQL は (1) 非終端行のみを (2) deadline 超過のものに限り
    /// failed に倒し、(3) tenant_id をバインドパラメータで境界し、(4) 単一 UPDATE である。
    /// DB 非依存で SQL テキストを静的検査する（退行ガード）。
    #[test]
    fn stuck_execution_sweep_sql_is_guarded() {
        // 非終端（pending/running）行のみを対象にする（既終端は触らない = CAS 的）。
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("status IN ('pending', 'running')"));
        // failed へ倒す。
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("status = 'failed'"));
        // deadline 超過（created_at < now() - interval）に限定する。
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("created_at <"));
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("make_interval"));
        // tenant_id / deadline はバインドパラメータ（テナント境界 + injection 防止）。
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("tenant_id = $1"));
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("$2"));
        // 回収した id を返す（呼び出し側が DECR する）。
        assert!(STUCK_EXECUTION_SWEEP_SQL.contains("RETURNING id"));
        // 単一 UPDATE（DELETE を伴わない）。
        let upper = STUCK_EXECUTION_SWEEP_SQL.to_ascii_uppercase();
        assert!(upper.starts_with("UPDATE EXECUTIONS"));
        assert!(!upper.contains("DELETE"));
    }

    /// set_tenant_guc の SQL は **必ず** `$1` バインドプレースホルダで tenant_id を渡し、
    /// SET ステートメントを使わず set_config(..., true)（transaction-local）を使う。
    /// （RLS の fail-closed と injection 防止の不変条件。DB 非依存で検査する。）
    #[test]
    fn set_tenant_guc_sql_is_parameterized() {
        // パラメータバインドを使う（$1 がある）。
        assert!(
            SET_TENANT_GUC_SQL.contains("$1"),
            "tenant_id must be passed as a bind parameter, not interpolated"
        );
        // set_config を transaction-local (3rd arg true) で呼ぶ。
        assert!(SET_TENANT_GUC_SQL.contains("set_config"));
        assert!(SET_TENANT_GUC_SQL.contains("'app.tenant_id'"));
        assert!(SET_TENANT_GUC_SQL.contains("true"));
        // 生の SET ステートメントは使わない（SET app.tenant_id = ... は文字列結合の温床）。
        let upper = SET_TENANT_GUC_SQL.to_ascii_uppercase();
        assert!(
            !upper.contains("SET APP.TENANT_ID"),
            "must not use a SET statement for the GUC"
        );
    }

    /// tenant_id 値は SQL 文字列に一切埋め込まれない（任意の値を入れても SQL は不変）。
    #[test]
    fn set_tenant_guc_sql_never_embeds_tenant_value() {
        // SQL はコンパイル時定数であり、tenant_id 引数に依存しない。
        // 代表的な injection ペイロードが SQL リテラルに現れないことを確認する。
        assert!(!SET_TENANT_GUC_SQL.contains("'; DROP"));
        assert!(!SET_TENANT_GUC_SQL.contains("tenant-123"));
    }

    // --- M3c migration 0005 不変条件（DB 非依存。SQL テキストを静的検査する）---------
    //
    // ライブ DB はこの環境では検証不能のため、追記専用 + RLS + 部分 UNIQUE の不変条件を
    // マイグレーション SQL のテキストに対して検査する（退行ガード）。

    const MIGRATION_0005: &str = include_str!("../../../migrations/0005_provenance.sql");

    /// audit_logs は追記専用: faas_app に UPDATE/DELETE を **GRANT しない**こと（§3.2 MUST NOT）。
    #[test]
    fn audit_logs_is_append_only_for_faas_app() {
        let sql = MIGRATION_0005;
        // 防御的 REVOKE は存在してよいが、GRANT 側に UPDATE/DELETE が紛れていないこと。
        // GRANT 行を集めて、その中に UPDATE/DELETE が無いことを確認する。
        for line in sql.lines() {
            let stripped = line.split("--").next().unwrap_or("").to_ascii_uppercase();
            if stripped.contains("GRANT") && stripped.contains("AUDIT_LOGS") {
                assert!(
                    !stripped.contains("UPDATE"),
                    "audit_logs must never GRANT UPDATE: {line}"
                );
                assert!(
                    !stripped.contains("DELETE"),
                    "audit_logs must never GRANT DELETE: {line}"
                );
                assert!(
                    !stripped.contains(" ALL "),
                    "audit_logs must not GRANT ALL: {line}"
                );
            }
        }
        // 明示的に SELECT,INSERT は付与する。
        assert!(sql.contains("GRANT  SELECT, INSERT ON audit_logs TO   faas_app"));
        // 防御的に UPDATE/DELETE を REVOKE し、PUBLIC からも全剥奪する。
        assert!(sql
            .to_ascii_uppercase()
            .contains("REVOKE UPDATE, DELETE ON AUDIT_LOGS FROM FAAS_APP"));
        assert!(sql
            .to_ascii_uppercase()
            .contains("REVOKE ALL            ON AUDIT_LOGS FROM PUBLIC"));
    }

    /// audit_logs は 0004 と同形の FORCE RLS + fail-closed tenant_isolation を持つこと。
    #[test]
    fn audit_logs_has_force_rls_and_failclosed_policy() {
        let sql = MIGRATION_0005;
        assert!(sql.contains("ALTER TABLE audit_logs ENABLE ROW LEVEL SECURITY"));
        assert!(sql.contains("ALTER TABLE audit_logs FORCE  ROW LEVEL SECURITY"));
        assert!(sql.contains("CREATE POLICY tenant_isolation ON audit_logs"));
        // USING / WITH CHECK 両方を持つ。
        assert!(sql.contains("USING (tenant_id = current_setting('app.tenant_id'))"));
        assert!(sql.contains("WITH CHECK (tenant_id = current_setting('app.tenant_id'))"));
        // fail-closed: current_setting に第 2 引数（フォールバック）を付けない。
        assert!(
            !sql.contains("current_setting('app.tenant_id', true)"),
            "policy must be fail-closed: no 2nd-arg fallback on current_setting"
        );
    }

    /// 冪等性は部分 UNIQUE (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL（§6.6）。
    #[test]
    fn idempotency_partial_unique_index_present() {
        let sql = MIGRATION_0005;
        assert!(sql.contains("CREATE UNIQUE INDEX IF NOT EXISTS uq_executions_tenant_idem"));
        assert!(sql.contains("ON executions (tenant_id, idempotency_key)"));
        assert!(sql.contains("WHERE idempotency_key IS NOT NULL"));
    }

    /// 追加列は全て nullable な additive ADD COLUMN IF NOT EXISTS（backfill 不要）。
    #[test]
    fn executions_provenance_columns_are_additive() {
        let sql = MIGRATION_0005;
        for col in [
            "idempotency_key",
            "idempotency_request_hash",
            "job_token_kid",
        ] {
            assert!(
                sql.contains(&format!(
                    "ALTER TABLE executions ADD COLUMN IF NOT EXISTS {col}"
                )),
                "missing additive ADD COLUMN for {col}"
            );
        }
        // nullable: NOT NULL を付けない（DEFAULT も不要）。
        assert!(
            !sql.to_ascii_uppercase()
                .contains("ADD COLUMN IF NOT EXISTS IDEMPOTENCY_KEY          TEXT NOT NULL"),
            "provenance columns must stay nullable (additive, no backfill)"
        );
    }

    /// insert_audit_log の INSERT は audit_logs に対しパラメータ化されていること
    /// （tenant_id を含む全値を $n バインドで渡し、文字列結合しない）。
    /// この関数のクエリ文字列をテスト内で複製し、不変条件を静的に検査する。
    #[test]
    fn audit_log_insert_is_parameterized() {
        // insert_audit_log 本体と同一の SQL 文字列。
        let sql = "INSERT INTO audit_logs (tenant_id, actor, action, target, detail) \
                   VALUES ($1, $2, $3, $4, $5)";
        assert!(sql.contains("$1") && sql.contains("$5"));
        // INSERT のみ（UPDATE/DELETE しない＝追記専用）。
        let upper = sql.to_ascii_uppercase();
        assert!(upper.starts_with("INSERT INTO AUDIT_LOGS"));
        assert!(!upper.contains("UPDATE") && !upper.contains("DELETE"));
    }

    // ---- M7a: 解決 SQL の不変条件（DB-free 文字列検査） ----------------------

    /// 解決 SQL は必ずテナントで絞り、値を文字列結合しない（RLS の二重防御の片側）。
    #[test]
    fn resolve_routing_sql_is_tenant_scoped() {
        assert!(
            RESOLVE_ROUTING_SQL.contains("c.tenant_id = $1"),
            "resolution must be tenant-scoped in the predicate as well as under RLS"
        );
        assert!(
            RESOLVE_ROUTING_SQL.contains("c.name = $2"),
            "component name must be bound, never concatenated"
        );
        assert!(
            !RESOLVE_ROUTING_SQL.contains("format!") && !RESOLVE_ROUTING_SQL.contains("{}"),
            "resolution SQL must not interpolate values"
        );
    }

    /// canary 側 LEFT JOIN の 4 条件（id / tenant / component / 未削除）が揃っていること。
    /// どれか 1 つでも欠けると「壊れたポインタは fail-safe に stable へ倒れる」という
    /// 設計の中心主張が成立しなくなる。
    #[test]
    fn resolve_routing_sql_fails_safe_on_bad_canary() {
        for cond in [
            "cvv.id = c.canary_version_id",
            "cvv.tenant_id = c.tenant_id",
            "cvv.component_id = c.id",
            "cvv.deleted_at IS NULL",
        ] {
            assert!(
                RESOLVE_ROUTING_SQL.contains(cond),
                "canary LEFT JOIN must constrain `{cond}` (fail-safe to stable otherwise)"
            );
        }
    }

    /// stable 側の述語は M6 までの `active_version_storage` と**同値**であること
    /// （alias は `cv` → `sv` に改名しているので文字単位では一致しない。凍結リテラルで照合する）。
    /// `sv.component_id = c.id` は一貫データでは no-op の narrowing で、壊れたポインタのときだけ
    /// 「行なし → 既存の 400 has no active version」へ倒す（別 component の wasm を実行しない）。
    #[test]
    fn resolve_routing_sql_preserves_stable_predicates() {
        for cond in [
            "sv.id = c.active_version_id",
            "sv.tenant_id = c.tenant_id",
            "sv.component_id = c.id",
            "c.deleted_at IS NULL",
            "c.active_version_id IS NOT NULL",
        ] {
            assert!(
                RESOLVE_ROUTING_SQL.contains(cond),
                "stable-side predicate `{cond}` must be preserved"
            );
        }
    }

    // ---- M7a: stable ポインタを動かす 3 操作の不変条件 -----------------------

    /// stable を動かす操作は必ず canary をクリアする（新 stable と古い配分の混線を構造的に防ぐ）。
    #[test]
    fn switch_active_version_clears_canary() {
        for sql in [
            SWITCH_ACTIVE_VERSION_SQL,
            PROMOTE_ACTIVE_VERSION_SQL,
            ROLLBACK_ACTIVE_VERSION_SQL,
        ] {
            assert!(
                sql.contains("canary_version_id = NULL") && sql.contains("canary_weight = 0"),
                "moving the stable pointer must always clear the canary split"
            );
        }
    }

    /// 同じ版を再 activate しても `previous_active_version_id` を自分自身で潰さない。
    /// 潰すと直後の rollback が「200 を返すのに何も戻らない」最悪の failure mode になる。
    #[test]
    fn switch_active_version_preserves_previous_on_noop() {
        for sql in [
            SWITCH_ACTIVE_VERSION_SQL,
            PROMOTE_ACTIVE_VERSION_SQL,
            ROLLBACK_ACTIVE_VERSION_SQL,
        ] {
            assert!(
                sql.contains("CASE") && sql.contains("IS DISTINCT FROM"),
                "previous_active_version_id must be guarded by a no-op CASE"
            );
        }
    }

    /// promote は事前 SELECT 無しの単一 UPDATE + CAS（read-then-write のレースを作らない）。
    #[test]
    fn promote_sql_is_single_statement_cas() {
        assert!(
            PROMOTE_ACTIVE_VERSION_SQL.contains("canary_version_id IS NOT NULL"),
            "promote must refuse when no canary is configured"
        );
        assert!(
            PROMOTE_ACTIVE_VERSION_SQL.contains("($3 IS NULL OR canary_version_id = $3)"),
            "an explicit target must be a CAS condition, not a read-then-write"
        );
        assert!(
            PROMOTE_ACTIVE_VERSION_SQL.contains("RETURNING"),
            "promote must report the resulting pointers from the same statement"
        );
        assert!(
            !PROMOTE_ACTIVE_VERSION_SQL.contains("SELECT"),
            "the success path must stay a single UPDATE"
        );
    }

    /// rollback の戻り先は必ず**未削除の**version として解決する（tombstone を active にしない）。
    #[test]
    fn rollback_sql_requires_live_version() {
        assert!(
            ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.deleted_at IS NULL"),
            "rollback must not resurrect a soft-deleted version"
        );
        assert!(
            ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.component_id = c.id")
                && ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.tenant_id = c.tenant_id"),
            "rollback target must belong to the same component and tenant"
        );
        assert!(
            ROLLBACK_ACTIVE_VERSION_SQL.contains("COALESCE($3, c.previous_active_version_id)"),
            "an empty body must roll back to the recorded previous stable"
        );
    }
}
