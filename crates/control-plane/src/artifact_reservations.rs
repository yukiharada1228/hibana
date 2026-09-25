//! Durable upload journal and short-lived cache pins across publication and rollback.
use crate::{db, error::AppError, state::AppState};
use hibana_database::prelude::*;
use hibana_shared::FaasError;

pub(crate) struct Reservation {
    state: AppState,
    tenant: String,
    id: String,
    confirmed: bool,
    armed: bool,
}

impl Reservation {
    pub(crate) async fn new(
        state: &AppState,
        tenant: &str,
        version: &str,
        storage_uri: &str,
        sha256: &str,
    ) -> Result<Self, AppError> {
        let id = hibana_shared::new_artifact_reservation_id();
        let tx = state.pool().begin().await?;
        crate::storage::compiled::protect_publication(&tx).await?;
        db::set_tenant_guc(&tx, tenant).await?;
        artifact_reservations::Entity::insert(artifact_reservations::ActiveModel {
            id: Set(id.clone()),
            tenant_id: Set(tenant.into()),
            version_id: Set(version.into()),
            storage_uri: Set(storage_uri.into()),
            wasm_sha256: Set(sha256.into()),
            ..Default::default()
        })
        .exec_without_returning(&tx)
        .await?;
        tx.commit().await?;
        Ok(Self {
            state: state.clone(),
            tenant: tenant.into(),
            id,
            confirmed: false,
            armed: true,
        })
    }

    /// PUT completed, or this is an already registered version. A failed/aborted PUT
    /// is left to the delayed sweeper: deleting while that PUT is in flight can leak it.
    pub(crate) fn confirm_object(&mut self) {
        self.confirmed = true;
    }

    pub(crate) async fn lock(&self, tx: &sea_orm::DatabaseTransaction) -> Result<(), AppError> {
        crate::storage::compiled::protect_publication(tx).await?;
        artifact_reservations::Entity::find_by_id(self.id.clone())
            .filter(artifact_reservations::Column::TenantId.eq(&self.tenant))
            .filter(
                Expr::col((
                    artifact_reservations::Entity,
                    artifact_reservations::Column::ExpiresAt,
                ))
                // The advisory guard may have waited behind GC. Check wall time
                // after that wait, not the transaction's earlier start time.
                .gt(Func::cust("clock_timestamp")),
            )
            .lock_exclusive()
            .one(tx)
            .await?
            .ok_or(FaasError::Unavailable)?;
        Ok(())
    }

    pub(crate) async fn finish(mut self) {
        self.armed = false;
        if self.confirmed
            && cleanup(&self.state, &self.tenant, &self.id, false)
                .await
                .is_err()
        {
            tracing::warn!("Artifact reservation cleanup deferred to sweeper");
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.armed || !self.confirmed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (state, tenant, id) = (self.state.clone(), self.tenant.clone(), self.id.clone());
            runtime.spawn(async move {
                if cleanup(&state, &tenant, &id, false).await.is_err() {
                    tracing::warn!("Abandoned artifact cleanup deferred to sweeper");
                }
            });
        }
    }
}

async fn cleanup(
    state: &AppState,
    tenant: &str,
    id: &str,
    expired_only: bool,
) -> Result<(), AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let mut query = artifact_reservations::Entity::find_by_id(id)
        .filter(artifact_reservations::Column::TenantId.eq(tenant));
    if expired_only {
        query = query.filter(
            Expr::col((
                artifact_reservations::Entity,
                artifact_reservations::Column::ExpiresAt,
            ))
            .lte(now()),
        );
    }
    let row = query.lock_exclusive().one(&tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    // Publication locks this same journal row until COMMIT. Even if the commit's
    // acknowledgement is lost, never delete an artifact referenced by a version.
    let registered = component_versions::Entity::find_by_id(row.version_id.clone())
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::StorageUri.eq(&row.storage_uri))
        .count(&tx)
        .await?
        > 0;
    if !registered {
        state.storage().delete_object(&row.storage_uri).await?;
    }
    artifact_reservations::Entity::delete_many()
        .filter(artifact_reservations::Column::TenantId.eq(tenant))
        .filter(artifact_reservations::Column::Id.eq(id))
        .exec(&tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn sweep(state: &AppState) -> Result<(), AppError> {
    for (tenant, _) in db::list_tenants_for_admin(state.pool()).await? {
        let ids = match expired(state, &tenant).await {
            Ok(ids) => ids,
            Err(_) => {
                tracing::warn!(%tenant, "Artifact journal scan failed; continuing other tenants");
                continue;
            }
        };
        for id in ids {
            if cleanup(state, &tenant, &id, true).await.is_err() {
                tracing::warn!(%tenant, reservation = %id, "Artifact cleanup failed; continuing other reservations");
                if defer_cleanup(state, &tenant, &id).await.is_err() {
                    tracing::warn!(%tenant, reservation = %id, "Could not schedule artifact cleanup retry");
                }
            }
        }
    }
    Ok(())
}

async fn expired(state: &AppState, tenant: &str) -> Result<Vec<String>, AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let ids = artifact_reservations::Entity::find()
        .select_only()
        .column(artifact_reservations::Column::Id)
        .filter(artifact_reservations::Column::TenantId.eq(tenant))
        .filter(
            Expr::col((
                artifact_reservations::Entity,
                artifact_reservations::Column::ExpiresAt,
            ))
            .lte(now()),
        )
        .filter(
            Expr::col((
                artifact_reservations::Entity,
                artifact_reservations::Column::CleanupRetryAt,
            ))
            .lte(now()),
        )
        .order_by_asc(artifact_reservations::Column::CleanupRetryAt)
        .order_by_asc(artifact_reservations::Column::ExpiresAt)
        .order_by_asc(artifact_reservations::Column::Id)
        .limit(20)
        .into_tuple()
        .all(&tx)
        .await?;
    tx.commit().await?;
    Ok(ids)
}

async fn defer_cleanup(state: &AppState, tenant: &str, id: &str) -> Result<(), AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    // Do not change expires_at: a failed delete must not pin an orphan in caches.
    artifact_reservations::Entity::update_many()
        .col_expr(
            artifact_reservations::Column::CleanupRetryAt,
            now().add(Expr::cust("interval '30 seconds'")),
        )
        .filter(artifact_reservations::Column::TenantId.eq(tenant))
        .filter(artifact_reservations::Column::Id.eq(id))
        .filter(
            Expr::col((
                artifact_reservations::Entity,
                artifact_reservations::Column::ExpiresAt,
            ))
            .lte(now()),
        )
        .exec(&tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            if sweep(&state).await.is_err() {
                tracing::warn!("Artifact cleanup incomplete; will retry");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}
