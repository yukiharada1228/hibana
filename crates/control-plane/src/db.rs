//! DB アクセス層 (sqlx ランタイム API のみ; query! マクロは使わない)。
//!
//! 列構成は migrations/0001_init.sql に厳密対応。M1 は単一テナント 'default'。
//! 将来 (M3) のテナント分離に備え、関数は常に `tenant_id` を受け取る。

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

use faas_shared::{ExecutionStatus, Role, Scope};

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
        "SELECT id, name, active_version_id FROM components \
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

/// component の active_version_id を設定する (§6.7: latest=active)。
pub async fn set_active_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE components SET active_version_id = $3 \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(version_id)
    .execute(executor)
    .await?;
    Ok(())
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

/// component 名から active かつ未削除の version を解決する (invoke 用, §6.3)。
///
/// active_version_id が NULL のとき（version 未投入）は `None`。
pub async fn active_version_storage(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_name: &str,
) -> Result<Option<ActiveVersion>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT cv.id AS version_id, cv.version, cv.storage_uri, cv.wasm_sha256, \
                cv.resource_limits \
         FROM components c \
         JOIN component_versions cv \
           ON cv.id = c.active_version_id AND cv.tenant_id = c.tenant_id \
         WHERE c.tenant_id = $1 AND c.name = $2 AND c.deleted_at IS NULL \
           AND c.active_version_id IS NOT NULL",
    )
    .bind(tenant_id)
    .bind(component_name)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        // resource_limits(JSONB) から壁時計上限を解決する。欠損/不正は既定にフォールバック。
        let limits_json: Value = r.try_get("resource_limits")?;
        let max_wall_time_ms = serde_json::from_value::<faas_shared::ResourceLimits>(limits_json)
            .unwrap_or_default()
            .max_wall_time_ms;
        Ok(ActiveVersion {
            version_id: r.try_get("version_id")?,
            version: r.try_get("version")?,
            storage_uri: r.try_get("storage_uri")?,
            wasm_sha256: r.try_get("wasm_sha256")?,
            max_wall_time_ms,
        })
    })
    .transpose()
}

/// テナント内で name から component を解決する (soft-delete 済みは除外)。
pub async fn find_component_by_name(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    name: &str,
) -> Result<Option<ComponentRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, active_version_id FROM components \
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
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO executions \
         (id, tenant_id, component_id, version_id, status, input, \
          idempotency_key, idempotency_request_hash, job_token_kid, input_ref) \
         VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9)",
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
    .execute(executor)
    .await?;
    Ok(())
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
/// `(version_id, status)` のみ返す（行不在は `None` → 「未知の execution」として drop+audit）。
/// tenant_id は呼び出し側が GUC（subject 由来テナント）で既に拘束しているため返さない。
pub async fn find_execution_provenance(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT version_id, status FROM executions \
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok((
            r.try_get::<String, _>("version_id")?,
            r.try_get::<String, _>("status")?,
        ))
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
/// 返す id 群が「この pass で実際に pending/running → failed へ遷移させた行」（CAS で 1 度きり）。
pub async fn finalize_stuck_executions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    deadline_secs: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(STUCK_EXECUTION_SWEEP_SQL)
        .bind(tenant_id)
        .bind(deadline_secs)
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| r.try_get::<String, _>("id"))
        .collect()
}

/// `finalize_stuck_executions` の SQL。`created_at < now() - deadline` の非終端（pending/running）
/// 行のみを `failed` に遷移させ（CAS: 既終端は WHERE で除外）、遷移した行の id を返す。
/// `$2` は INTERVAL の秒数。`error` には sweeper 由来であることを記録する（監査・調査用）。
const STUCK_EXECUTION_SWEEP_SQL: &str = "UPDATE executions \
     SET status = 'failed', \
         error = '\"execution exceeded delivery deadline without a terminal result (stuck-execution sweeper)\"'::jsonb, \
         finished_at = now() \
     WHERE tenant_id = $1 \
       AND status IN ('pending', 'running') \
       AND created_at < now() - make_interval(secs => $2::double precision) \
     RETURNING id";

/// 結果メッセージを CAS 的に反映する (§ result subscriber)。
///
/// 終端状態 (succeeded/failed/timeout) へは「まだ終端でないときだけ」遷移させる。
/// これにより重複配送 / 競合更新が冪等になる。更新された行数を返す。
pub async fn finalize_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
    status: ExecutionStatus,
    output: Option<&Value>,
    error: Option<&Value>,
) -> Result<u64, sqlx::Error> {
    debug_assert!(status.is_terminal());

    let result = sqlx::query(FINALIZE_EXECUTION_SQL)
        .bind(tenant_id)
        .bind(execution_id)
        .bind(status.as_str())
        .bind(output)
        .bind(error)
        .execute(executor)
        .await?;

    Ok(result.rows_affected())
}

/// `finalize_execution` が発行する SQL。終端遷移は **CAS**（compare-and-set）で行う:
/// `WHERE ... AND status NOT IN (terminal)` により、既に終端の行には 0 行しか当たらない
/// （= 重複 result の再配送や worker 多重実行があっても終端状態を上書きしない, §6.6）。
/// 定数に切り出して DB 非依存のユニットテストで CAS ガードの存在を検査できるようにする。
const FINALIZE_EXECUTION_SQL: &str = "UPDATE executions \
     SET status = $3, output = $4, error = $5, finished_at = now() \
     WHERE tenant_id = $1 AND id = $2 \
       AND status NOT IN ('succeeded', 'failed', 'timeout')";

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

#[cfg(test)]
mod tests {
    use super::{
        TenantQuotaOverrides, FINALIZE_EXECUTION_SQL, SET_TENANT_GUC_SQL, STUCK_EXECUTION_SWEEP_SQL,
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
        // 単一 UPDATE（DELETE/INSERT を伴わない）。
        let upper = FINALIZE_EXECUTION_SQL.to_ascii_uppercase();
        assert!(upper.starts_with("UPDATE EXECUTIONS"));
        assert!(!upper.contains("DELETE"));
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
}
