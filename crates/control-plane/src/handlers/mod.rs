//! Management HTTP handlers grouped by resource. Authorization is attached by routes.rs.
use crate::error::AppError;
use hibana_shared::FaasError;
pub(crate) mod capabilities;
pub(crate) mod components;
pub(crate) mod configuration;
mod deployment;
pub(crate) mod executions;
pub(crate) mod health;
pub(crate) mod identity;
pub(crate) mod signing_keys;
pub(crate) mod tenants;
pub(crate) mod usage;

fn map_unique_violation(e: sea_orm::DbErr, msg: &str) -> AppError {
    if is_unique_violation(&e) {
        return FaasError::InvalidRequest(msg.to_string()).into();
    }
    e.into()
}

fn is_unique_violation(e: &sea_orm::DbErr) -> bool {
    matches!(
        e.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    )
}

#[cfg(test)]
mod tests;
