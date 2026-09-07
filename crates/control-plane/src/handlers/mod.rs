//! Management HTTP handlers grouped by resource. Authorization is attached by routes.rs.
use crate::error::AppError;
use faas_shared::FaasError;
pub(crate) mod capabilities;
pub(crate) mod components;
pub(crate) mod configuration;
pub(crate) mod executions;
pub(crate) mod health;
pub(crate) mod identity;
pub(crate) mod signing_keys;
pub(crate) mod tenants;
pub(crate) mod usage;

fn map_unique_violation(e: sqlx::Error, msg: &str) -> AppError {
    if let sqlx::Error::Database(db_err) = &e {
        // Postgres unique_violation = 23505
        if db_err.code().as_deref() == Some("23505") {
            return FaasError::InvalidRequest(msg.into()).into();
        }
    }
    e.into()
}

#[cfg(test)]
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505"))
}

#[cfg(test)]
mod tests;
