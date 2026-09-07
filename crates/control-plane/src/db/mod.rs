//! Tenant-aware SQL queries, grouped by domain. Callers own transaction boundaries.
//! Tenant-scoped operations run after `set_tenant_guc` on the same transaction.

mod components;
pub use components::*;
mod executions;
pub use executions::*;
mod signing_keys;
pub use signing_keys::*;
mod configuration;
pub use configuration::*;
mod secrets;
pub use secrets::*;
mod usage;
pub use usage::*;
mod identity;
pub use identity::*;
mod audit;
pub use audit::*;
mod tenants;
pub use tenants::*;

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

/// `u64` を `i64`（Postgres BIGINT）へ飽和変換する。計量は呼び出し側で `ResourceLimits` 上限に
/// clamp 済み（信頼境界外対策）だが、二重防御として `i64::MAX` で頭打ちにし、桁あふれによる
/// 負値混入や格納失敗を防ぐ（純関数・DB 非依存でテスト可能）。
fn saturating_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests;
