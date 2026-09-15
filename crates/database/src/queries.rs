//! Shared immutable version identity joins. All three identity columns matter.
use crate::prelude::*;

pub fn pinned_execution(tenant: &str, id: &str) -> sea_orm::Select<executions::Entity> {
    executions::Entity::find()
        .join(
            JoinType::InnerJoin,
            executions::Entity::belongs_to(component_versions::Entity)
                .from((
                    executions::Column::TenantId,
                    executions::Column::ComponentId,
                    executions::Column::VersionId,
                ))
                .to((
                    component_versions::Column::TenantId,
                    component_versions::Column::ComponentId,
                    component_versions::Column::Id,
                ))
                .into(),
        )
        .join(
            JoinType::InnerJoin,
            executions::Entity::belongs_to(components::Entity)
                .from((
                    executions::Column::TenantId,
                    executions::Column::ComponentId,
                ))
                .to((components::Column::TenantId, components::Column::Id))
                .into(),
        )
        .filter(executions::Column::TenantId.eq(tenant))
        .filter(executions::Column::Id.eq(id))
        .filter(executions::Column::Status.eq("pending"))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(components::Column::DeletedAt.is_null())
        .filter(component_versions::Column::DeletedAt.is_null())
}

pub fn live_versions(tenant: &str) -> sea_orm::Select<component_versions::Entity> {
    component_versions::Entity::find()
        .join(
            JoinType::InnerJoin,
            component_versions::Entity::belongs_to(components::Entity)
                .from((
                    component_versions::Column::TenantId,
                    component_versions::Column::ComponentId,
                ))
                .to((components::Column::TenantId, components::Column::Id))
                .into(),
        )
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::DeletedAt.is_null())
        .filter(components::Column::DeletedAt.is_null())
}

pub fn active_versions(tenant: &str) -> sea_orm::Select<component_versions::Entity> {
    live_versions(tenant).filter(
        Expr::col((components::Entity, components::Column::ActiveVersionId)).eq(Expr::col((
            component_versions::Entity,
            component_versions::Column::Id,
        ))),
    )
}

/// One snapshot includes every bound live Secret, even if its historical
/// generation is missing. The caller must reject missing envelopes.
pub async fn secret_envelopes(
    db: &impl ConnectionTrait,
    tenant: &str,
    component: &str,
    version: &str,
    created: chrono::DateTime<chrono::Utc>,
    names: &[String],
) -> Result<Vec<sea_orm::QueryResult>, DbErr> {
    use sea_orm::sea_query::{Asterisk, Order};
    let generation = Query::select()
        .column(Asterisk)
        .from_as(function_secret_versions::Entity, "v2")
        .and_where(
            Expr::col(("v2", function_secret_versions::Column::TenantId))
                .equals(("s", function_secrets::Column::TenantId)),
        )
        .and_where(
            Expr::col(("v2", function_secret_versions::Column::SecretId))
                .equals(("s", function_secrets::Column::Id)),
        )
        .and_where(Expr::col(("v2", function_secret_versions::Column::CreatedAt)).lte(created))
        .order_by(
            ("v2", function_secret_versions::Column::Version),
            Order::Desc,
        )
        .limit(1)
        .to_owned();
    let mut query = Query::select()
        .expr_as(Expr::col(("s", function_secrets::Column::Id)), "secret_id")
        .column(("s", function_secrets::Column::Name))
        .from_as(function_secrets::Entity, "s")
        .join_as(
            JoinType::InnerJoin,
            version_secret_bindings::Entity,
            "b",
            Condition::all()
                .add(
                    Expr::col(("b", version_secret_bindings::Column::TenantId))
                        .equals(("s", function_secrets::Column::TenantId)),
                )
                .add(
                    Expr::col(("b", version_secret_bindings::Column::ComponentId))
                        .equals(("s", function_secrets::Column::ComponentId)),
                )
                .add(
                    Expr::col(("b", version_secret_bindings::Column::SecretId))
                        .equals(("s", function_secrets::Column::Id)),
                )
                .add(
                    Expr::col(("b", version_secret_bindings::Column::Name))
                        .equals(("s", function_secrets::Column::Name)),
                )
                .add(Expr::col(("b", version_secret_bindings::Column::VersionId)).eq(version)),
        )
        .join_lateral(JoinType::LeftJoin, generation, "v", Expr::val(true))
        .and_where(Expr::col(("s", function_secrets::Column::TenantId)).eq(tenant))
        .and_where(Expr::col(("s", function_secrets::Column::ComponentId)).eq(component))
        .and_where(Expr::col(("s", function_secrets::Column::DeletedAt)).is_null())
        .and_where(Expr::col(("s", function_secrets::Column::Name)).is_in(names.to_vec()))
        .order_by(("s", function_secrets::Column::Name), Order::Asc)
        .to_owned();
    for column in [
        function_secret_versions::Column::Version,
        function_secret_versions::Column::KekKid,
        function_secret_versions::Column::WrappedDek,
        function_secret_versions::Column::DekNonce,
        function_secret_versions::Column::Nonce,
        function_secret_versions::Column::Ciphertext,
        function_secret_versions::Column::ValueLen,
    ] {
        query.column(("v", column));
    }
    db.query_all(&query).await
}
