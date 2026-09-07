//! Tenants persistence.
use serde_json::Value;
use sqlx::Row;

/// テナントが `active` かどうか (M7c: `/internal/job-env` は認証 middleware 外なので個別確認する)。
///
/// `auth::authenticate` が middleware で行っている停止テナント遮断と同じ判定を、middleware の
/// 外にある内部エンドポイントで**明示的に**行うためのもの（§4.6.1 (3) の MUST）。
/// RLS 下の GUC を必要としない参照なので、SECURITY DEFINER 関数と同じく GUC 前に呼べる。
pub async fn tenant_is_active(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1 AND status = 'active') AS ok",
    )
    .bind(tenant_id)
    .fetch_one(executor)
    .await?;
    row.try_get("ok")
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

/// M10 follow-up: テナントの実行ステータスを設定する（platform admin op, §3.2 / §9）。
///
/// `status` は呼び出し側で `active` / `suspended` に検証済みであること。0 行 = テナント不在 → 404。
/// `suspended` にすると `auth.rs` の middleware が以後のリクエストを 403 で弾く（次リクエストから即時）。
pub async fn set_tenant_status(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("UPDATE tenants SET status = $2 WHERE id = $1")
        .bind(tenant_id)
        .bind(status)
        .execute(executor)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// M10 follow-up: テナントのクォータ上書き（`tenants.quotas` JSONB）を全置換する（platform admin op, §8）。
///
/// `quotas` は呼び出し側で [`TenantQuotaOverrides`] 形へ検証・正規化済みの JSON であること。
/// 0 行 = テナント不在 → 404。次の invoke から `load_tenant_status_and_quotas` が新値を読む。
pub async fn set_tenant_quotas(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    quotas: &Value,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("UPDATE tenants SET quotas = $2 WHERE id = $1")
        .bind(tenant_id)
        .bind(quotas)
        .execute(executor)
        .await?;
    Ok(r.rows_affected() > 0)
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
