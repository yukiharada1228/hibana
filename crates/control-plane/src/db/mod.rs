//! Tenant-aware ORM queries, grouped by domain. Callers own transaction boundaries.
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

pub use hibana_database::postgres::set_tenant_guc;

/// `u64` を `i64`（Postgres BIGINT）へ飽和変換する。計量は呼び出し側で `ResourceLimits` 上限に
/// clamp 済み（信頼境界外対策）だが、二重防御として `i64::MAX` で頭打ちにし、桁あふれによる
/// 負値混入や格納失敗を防ぐ（純関数・DB 非依存でテスト可能）。
fn saturating_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests;
