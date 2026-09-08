//! Version-scoped environment. The caller holds the component publication lock.
use crate::{error::AppError, validation};
use hibana_shared::FaasError;
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct VersionEnvironment {
    pub vars: BTreeMap<String, String>,
    pub secrets: Vec<String>,
}

impl VersionEnvironment {
    pub fn validate(&self) -> Result<BTreeSet<String>, FaasError> {
        super::configuration::validate_env_map(&self.vars)?;
        let selected = validation::validate_env_allowlist(&self.secrets)?;
        if selected.len() != self.secrets.len() {
            return Err(FaasError::InvalidRequest("duplicate Secret names".into()));
        }
        if self.vars.keys().any(|key| selected.contains(key)) {
            return Err(FaasError::InvalidRequest(
                "vars and secrets must use distinct names".into(),
            ));
        }
        let names: Vec<_> = self.vars.keys().cloned().chain(selected).collect();
        validation::validate_env_allowlist(&names)
    }

    pub async fn save(
        &self,
        tx: &mut sqlx::PgConnection,
        tenant: &str,
        component: &str,
        version: &str,
    ) -> Result<(), AppError> {
        // Secret administration also takes the component lock. FOR SHARE additionally
        // serializes with metadata rotation/rekey. Never trust a caller's env capabilities.
        let rows = sqlx::query(
            "SELECT id, name, deploy_allowed FROM function_secrets \
             WHERE tenant_id=$1 AND component_id=$2 AND deleted_at IS NULL ORDER BY name FOR SHARE",
        )
        .bind(tenant)
        .bind(component)
        .fetch_all(&mut *tx)
        .await?;
        for row in &rows {
            let name: &str = row.try_get("name")?;
            if self.vars.contains_key(name) {
                return Err(
                    FaasError::Conflict("a var conflicts with a stored Secret".into()).into(),
                );
            }
        }
        for name in &self.secrets {
            let row = rows.iter().find(|r| r.get::<&str, _>("name") == name);
            let row = row
                .filter(|r| r.get::<bool, _>("deploy_allowed"))
                .ok_or(FaasError::Forbidden)?;
            sqlx::query("INSERT INTO version_secret_bindings (tenant_id,component_id,version_id,secret_id,name) VALUES ($1,$2,$3,$4,$5)")
                .bind(tenant).bind(component).bind(version).bind(row.get::<&str, _>("id")).bind(name)
                .execute(&mut *tx).await?;
        }
        for (key, value) in &self.vars {
            sqlx::query("INSERT INTO version_configs (tenant_id,component_id,version_id,key,value) VALUES ($1,$2,$3,$4,$5)")
                .bind(tenant).bind(component).bind(version).bind(key).bind(value)
                .execute(&mut *tx).await?;
        }
        Ok(())
    }
}
