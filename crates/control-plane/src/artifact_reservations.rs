//! Durable upload journal and short-lived cache pins across publication and rollback.
use crate::{db, error::AppError, state::AppState};
use hibana_shared::FaasError;
use sqlx::Row;

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
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, tenant).await?;
        sqlx::query("INSERT INTO artifact_reservations(id,tenant_id,version_id,storage_uri,wasm_sha256) VALUES($1,$2,$3,$4,$5)")
            .bind(&id).bind(tenant).bind(version).bind(storage_uri).bind(sha256)
            .execute(&mut *tx).await?;
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

    pub(crate) async fn lock(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), AppError> {
        sqlx::query("SELECT id FROM artifact_reservations WHERE tenant_id=$1 AND id=$2 AND expires_at > now() FOR UPDATE")
            .bind(&self.tenant).bind(&self.id).fetch_optional(&mut **tx).await?
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
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = sqlx::query("SELECT version_id,storage_uri FROM artifact_reservations WHERE tenant_id=$1 AND id=$2 AND (NOT $3 OR expires_at <= now()) FOR UPDATE")
        .bind(tenant).bind(id).bind(expired_only).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    // Publication locks this same journal row until COMMIT. Even if the commit's
    // acknowledgement is lost, never delete an artifact referenced by a version.
    let registered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM component_versions WHERE tenant_id=$1 AND id=$2 AND storage_uri=$3)")
        .bind(tenant).bind(row.get::<String,_>("version_id")).bind(row.get::<String,_>("storage_uri"))
        .fetch_one(&mut *tx).await?;
    if !registered {
        state
            .storage()
            .delete_object(&row.get::<String, _>("storage_uri"))
            .await?;
    }
    sqlx::query("DELETE FROM artifact_reservations WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .execute(&mut *tx)
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
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let ids = sqlx::query_scalar("SELECT id FROM artifact_reservations WHERE tenant_id=$1 AND expires_at <= now() AND cleanup_retry_at <= now() ORDER BY cleanup_retry_at,expires_at,id LIMIT 20")
        .bind(tenant).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ids)
}

async fn defer_cleanup(state: &AppState, tenant: &str, id: &str) -> Result<(), AppError> {
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    // Do not change expires_at: a failed delete must not pin an orphan in caches.
    sqlx::query("UPDATE artifact_reservations SET cleanup_retry_at=now()+interval '30 seconds' WHERE tenant_id=$1 AND id=$2 AND expires_at <= now()")
        .bind(tenant).bind(id).execute(&mut *tx).await?;
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
