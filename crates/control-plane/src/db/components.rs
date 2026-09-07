//! Components persistence.
use chrono::DateTime;
use chrono::Utc;
use serde_json::Value;
use sqlx::Row;

/// `components` の 1 行 (API 応答に必要な範囲)。
#[derive(Debug, Clone)]
pub struct ComponentRow {
    pub id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub previous_active_version_id: Option<String>,
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
        "SELECT id, name, active_version_id, previous_active_version_id \
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

pub(super) const SWITCH_ACTIVE_VERSION_SQL: &str = "UPDATE components \
        SET previous_active_version_id = CASE \
                WHEN active_version_id IS DISTINCT FROM $3 THEN active_version_id \
                ELSE previous_active_version_id END, \
            active_version_id = $3 \
      WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL";

pub(super) const ROLLBACK_ACTIVE_VERSION_SQL: &str = "UPDATE components c \
        SET previous_active_version_id = CASE \
                WHEN c.active_version_id IS DISTINCT FROM cv.id \
                THEN c.active_version_id ELSE c.previous_active_version_id END, \
            active_version_id = cv.id \
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

/// M11 (§4.2): 公開 ingress gateway 用の component 解決結果。
/// gateway は「到達可否」だけ判定し、実際の起動は名前で通常 invoke に委ねる（id は不要）。
pub struct IngressComponentRow {
    /// 公開 URL から到達を許すか（deny-by-default）。
    pub ingress_enabled: bool,
    /// active な版（無ければ実行不可）。
    pub active_version_id: Option<String>,
}

/// M11 (§4.2): テナント内の component を名前で引き、公開 ingress に必要な列だけ返す。
/// FORCE RLS 下なので呼び出し側は `set_tenant_guc(tenant)` 済みの tx を渡すこと。
pub async fn find_ingress_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    name: &str,
) -> Result<Option<IngressComponentRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT ingress_enabled, active_version_id \
         FROM components \
         WHERE tenant_id = $1 AND name = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(name)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(IngressComponentRow {
            ingress_enabled: r.try_get("ingress_enabled")?,
            active_version_id: r.try_get("active_version_id")?,
        })
    })
    .transpose()
}

/// M11 (§4.2): component の公開 ingress opt-in フラグを設定する。戻り値は行が在って
/// 更新できたか（存在しない/削除済みは false）。GUC 済み tx を渡すこと（FORCE RLS）。
pub async fn set_component_ingress(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    enabled: bool,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE components SET ingress_enabled = $3 \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(enabled)
    .execute(executor)
    .await?;
    Ok(res.rows_affected() > 0)
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
