//! Combined vars/Secret sizes. Callers hold the component publication lock.
//! Only Secret length metadata is read; plaintext and ciphertext stay out of this query.
use hibana_database::prelude::*;
use sea_orm::sea_query::{Alias, SelectStatement, UnionType};

pub async fn version_environment_within_limit(
    tx: &sea_orm::DatabaseTransaction,
    tenant: &str,
    component: &str,
    version: &str,
) -> Result<bool, DbErr> {
    let versions = component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Id)
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::ComponentId.eq(component))
        .filter(component_versions::Column::Id.eq(version))
        .into_query();
    environments_within_limit(tx, tenant, component, versions).await
}

pub async fn secret_environments_within_limit(
    tx: &sea_orm::DatabaseTransaction,
    tenant: &str,
    component: &str,
    secret: &str,
) -> Result<bool, DbErr> {
    // Protect inactive versions too: they can still be selected for rollback.
    let bindings = version_secret_bindings::Entity::find()
        .select_only()
        .column(version_secret_bindings::Column::VersionId)
        .filter(version_secret_bindings::Column::TenantId.eq(tenant))
        .filter(version_secret_bindings::Column::ComponentId.eq(component))
        .filter(version_secret_bindings::Column::SecretId.eq(secret))
        .into_query();
    let versions = component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Id)
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::ComponentId.eq(component))
        .filter(component_versions::Column::DeletedAt.is_null())
        .filter(component_versions::Column::Id.in_subquery(bindings))
        .into_query();
    environments_within_limit(tx, tenant, component, versions).await
}

async fn environments_within_limit(
    tx: &sea_orm::DatabaseTransaction,
    tenant: &str,
    component: &str,
    versions: SelectStatement,
) -> Result<bool, DbErr> {
    // PostgreSQL octet_length matches Rust String::len(), including UTF-8 vars.
    let var_bytes = Expr::expr(
        Func::cust(Alias::new("octet_length")).arg(Expr::col(version_configs::Column::Key)),
    )
    .add(Func::cust(Alias::new("octet_length")).arg(Expr::col(version_configs::Column::Value)));
    let mut sizes = version_configs::Entity::find()
        .select_only()
        .column(version_configs::Column::VersionId)
        .column_as(var_bytes, "bytes")
        .filter(version_configs::Column::TenantId.eq(tenant))
        .filter(version_configs::Column::ComponentId.eq(component))
        .filter(version_configs::Column::VersionId.in_subquery(versions.clone()))
        .into_query();
    let secret_bytes = Expr::expr(Func::cust(Alias::new("octet_length")).arg(Expr::col((
        version_secret_bindings::Entity,
        version_secret_bindings::Column::Name,
    ))))
    .add(Expr::col((
        function_secret_versions::Entity,
        function_secret_versions::Column::ValueLen,
    )));
    let secrets = version_secret_bindings::Entity::find()
        .join(
            JoinType::InnerJoin,
            version_secret_bindings::Entity::belongs_to(function_secrets::Entity)
                .from((
                    version_secret_bindings::Column::TenantId,
                    version_secret_bindings::Column::ComponentId,
                    version_secret_bindings::Column::SecretId,
                ))
                .to((
                    function_secrets::Column::TenantId,
                    function_secrets::Column::ComponentId,
                    function_secrets::Column::Id,
                ))
                .into(),
        )
        .join(
            JoinType::InnerJoin,
            function_secrets::Entity::belongs_to(function_secret_versions::Entity)
                .from((
                    function_secrets::Column::TenantId,
                    function_secrets::Column::Id,
                    function_secrets::Column::CurrentVersion,
                ))
                .to((
                    function_secret_versions::Column::TenantId,
                    function_secret_versions::Column::SecretId,
                    function_secret_versions::Column::Version,
                ))
                .into(),
        )
        .select_only()
        .column(version_secret_bindings::Column::VersionId)
        .column_as(secret_bytes, "bytes")
        .filter(version_secret_bindings::Column::TenantId.eq(tenant))
        .filter(version_secret_bindings::Column::ComponentId.eq(component))
        .filter(version_secret_bindings::Column::VersionId.in_subquery(versions))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .into_query();
    // Keep equal-sized entries, aggregate once per version, and return at most
    // one marker. Empty environments have no violating group and remain valid.
    sizes.union(UnionType::All, secrets);
    Ok(tx
        .query_one(
            &Query::select()
                .expr(Expr::val(1))
                .from_subquery(sizes, "environment_sizes")
                .group_by_col("version_id")
                .cond_having(
                    Expr::col("bytes")
                        .sum()
                        .not_between(0_i64, hibana_shared::MAX_FUNCTION_ENV_TOTAL_BYTES as i64),
                )
                .limit(1)
                .to_owned(),
        )
        .await?
        .is_none())
}
