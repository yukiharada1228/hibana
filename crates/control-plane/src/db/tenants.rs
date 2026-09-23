//! Tenants persistence.
use hibana_database::prelude::*;

use serde_json::Value;

/// Platform-admin inventory; tenant-scoped rows are still read under their own GUC.
pub async fn list_tenants_for_admin(
    pool: &DatabaseConnection,
) -> Result<Vec<(String, String)>, DbErr> {
    tenants::Entity::find()
        .select_only()
        .columns([tenants::Column::Id, tenants::Column::Slug])
        .order_by_asc(tenants::Column::Slug)
        .into_tuple()
        .all(pool)
        .await
}

/// テナントが `active` かどうか (M7c: `/internal/job-env` は認証 middleware 外なので個別確認する)。
///
/// `auth::authenticate` が middleware で行っている停止テナント遮断と同じ判定を、middleware の
/// 外にある内部エンドポイントで**明示的に**行うためのもの（§4.6.1 (3) の MUST）。
/// RLS 下の GUC を必要としない参照なので、SECURITY DEFINER 関数と同じく GUC 前に呼べる。
pub async fn tenant_is_active(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<bool, DbErr> {
    hibana_database::queries::exists(
        executor,
        tenants::Entity::find_by_id(tenant_id).filter(tenants::Column::Status.eq("active")),
    )
    .await
}

/// M4d (§8): テナント別クォータ上書き値。`tenants.quotas` JSONB から読む。
///
/// 値がキーごとに `Option<u64>` なのは、欠損 / null を「グローバル既定を継承」として
/// 扱うため（仕様 §8: グローバル既定 → テナント上書きの優先順位で解決）。未知キー
/// （例: 将来追加されるクォータ）は無視する（前方互換）。serde は `deny_unknown_fields`
/// を **使わない** —— 将来追加されたフィールドで起動中の CP が解釈エラーを返さない
/// よう緩く受ける（例: invoke_rate_per_sec / invoke_burst /
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
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<Option<(String, TenantQuotaOverrides)>, DbErr> {
    let Some((status, quotas)) = tenants::Entity::find_by_id(tenant_id)
        .select_only()
        .columns([tenants::Column::Status, tenants::Column::Quotas])
        .into_tuple::<(String, Value)>()
        .one(executor)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((
        status,
        serde_json::from_value(quotas).unwrap_or_default(),
    )))
}

/// M10 follow-up: テナントの実行ステータスを設定する（platform admin op, §3.2 / §9）。
///
/// `status` は呼び出し側で `active` / `suspended` に検証済みであること。0 行 = テナント不在 → 404。
/// `suspended` にすると `auth.rs` の middleware が以後のリクエストを 403 で弾く（次リクエストから即時）。
pub async fn set_tenant_status(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    status: &str,
) -> Result<bool, DbErr> {
    Ok(tenants::Entity::update_many()
        .col_expr(tenants::Column::Status, Expr::val(status))
        .filter(tenants::Column::Id.eq(tenant_id))
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// M10 follow-up: テナントのクォータ上書き（`tenants.quotas` JSONB）を全置換する（platform admin op, §8）。
///
/// `quotas` は呼び出し側で [`TenantQuotaOverrides`] 形へ検証・正規化済みの JSON であること。
/// 0 行 = テナント不在 → 404。次の invoke から `load_tenant_status_and_quotas` が新値を読む。
pub async fn set_tenant_quotas(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    quotas: &Value,
) -> Result<bool, DbErr> {
    Ok(tenants::Entity::update_many()
        .col_expr(tenants::Column::Quotas, Expr::val(quotas.clone()))
        .filter(tenants::Column::Id.eq(tenant_id))
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}
