//! Version-scoped environment. The caller holds the component publication lock.
use crate::{error::AppError, validation};
use hibana_database::prelude::*;
use hibana_shared::FaasError;
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
        tx: &sea_orm::DatabaseTransaction,
        tenant: &str,
        component: &str,
        version: &str,
    ) -> Result<(), AppError> {
        let rows = function_secrets::Entity::find()
            .filter(function_secrets::Column::TenantId.eq(tenant))
            .filter(function_secrets::Column::ComponentId.eq(component))
            .filter(function_secrets::Column::DeletedAt.is_null())
            .order_by_asc(function_secrets::Column::Name)
            .lock_shared()
            .all(tx)
            .await?;
        for row in &rows {
            if self.vars.contains_key(&row.name) {
                return Err(
                    FaasError::Conflict("a var conflicts with a stored Secret".into()).into(),
                );
            }
        }
        let mut bindings = Vec::with_capacity(self.secrets.len());
        for name in &self.secrets {
            let row = rows
                .iter()
                .find(|r| &r.name == name && r.deploy_allowed)
                .ok_or(FaasError::Forbidden)?;
            bindings.push(version_secret_bindings::ActiveModel {
                tenant_id: Set(tenant.into()),
                component_id: Set(component.into()),
                version_id: Set(version.into()),
                secret_id: Set(row.id.clone()),
                name: Set(name.clone()),
            });
        }
        // SeaORM skips empty batches and keeps each nonempty table to one INSERT.
        version_secret_bindings::Entity::insert_many(bindings)
            .exec_without_returning(tx)
            .await?;
        version_configs::Entity::insert_many(self.vars.iter().map(|(key, value)| {
            version_configs::ActiveModel {
                tenant_id: Set(tenant.into()),
                component_id: Set(component.into()),
                version_id: Set(version.into()),
                key: Set(key.clone()),
                value: Set(value.clone()),
                ..Default::default()
            }
        }))
        .exec_without_returning(tx)
        .await?;
        if !crate::db::version_environment_within_limit(tx, tenant, component, version).await? {
            return Err(FaasError::InvalidRequest(
                "vars and selected Secrets together exceed the environment size limit".into(),
            )
            .into());
        }
        Ok(())
    }
}
