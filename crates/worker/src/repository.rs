//! Tenant-scoped ORM access to pinned version configuration and current egress.
use hibana_database::{prelude::*, queries::pinned_execution};
use hibana_shared::ResourceLimits;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub(crate) struct ResolvedVersion {
    pub(crate) limits: ResourceLimits,
    pub(crate) allowed_env: BTreeSet<String>,
    pub(crate) allow_outbound: Vec<hibana_shared::egress::EgressEndpoint>,
    pub(crate) config: BTreeMap<String, String>,
}

#[derive(FromQueryResult)]
struct PinnedVersion {
    component_id: String,
    version_id: String,
    resource_limits: serde_json::Value,
    capabilities: serde_json::Value,
    egress_policy: serde_json::Value,
}

pub(crate) struct ExecutionRepository {
    pool: DatabaseConnection,
}
impl ExecutionRepository {
    pub(crate) fn new(pool: DatabaseConnection) -> Self {
        Self { pool }
    }

    pub(crate) async fn active_artifact_hashes(&self) -> Result<BTreeSet<String>, DbErr> {
        Ok(hibana_database::queries::active_artifacts(&self.pool)
            .await?
            .into_iter()
            .map(|artifact| artifact.sha256)
            .collect())
    }

    pub(crate) async fn mark_running(&self, tenant: &str, id: &str) -> anyhow::Result<bool> {
        let started = std::time::Instant::now();
        let tx = self.pool.begin().await?;
        set_tenant_guc(&tx, tenant).await?;
        let changed = executions::Entity::update_many()
            .col_expr(executions::Column::Status, Expr::val("running"))
            .col_expr(
                executions::Column::StartedAt,
                Func::coalesce([Expr::col(executions::Column::StartedAt), now()]).into(),
            )
            .filter(executions::Column::TenantId.eq(tenant))
            .filter(executions::Column::Id.eq(id))
            .filter(executions::Column::Status.eq("pending"))
            .filter(executions::Column::HttpRequest.eq(true))
            .exec(&tx)
            .await?
            .rows_affected;
        tx.commit().await?;
        tracing::debug!(target:"hibana_latency",execution_id=%id,stage="worker_claim_db",elapsed_us=started.elapsed().as_micros() as u64,"HTTP phase timing");
        Ok(changed == 1)
    }

    pub(crate) async fn resolve_execution(
        &self,
        tenant: &str,
        id: &str,
        sha256: &str,
    ) -> anyhow::Result<ResolvedVersion> {
        let tx = self.pool.begin().await?;
        set_tenant_guc(&tx, tenant).await?;
        let row = pinned_execution(tenant, id)
            .filter(component_versions::Column::WasmSha256.eq(sha256))
            .select_only()
            .column(executions::Column::ComponentId)
            .column(executions::Column::VersionId)
            .column_as(
                Expr::col((
                    component_versions::Entity,
                    component_versions::Column::ResourceLimits,
                )),
                "resource_limits",
            )
            .column_as(
                Expr::col((
                    component_versions::Entity,
                    component_versions::Column::Capabilities,
                )),
                "capabilities",
            )
            .column_as(
                Expr::col((components::Entity, components::Column::EgressPolicy)),
                "egress_policy",
            )
            .into_model::<PinnedVersion>()
            .one(&tx)
            .await?
            .ok_or_else(|| anyhow::anyhow!("version configuration missing"))?;
        let allowed_env = hibana_shared::capabilities::allowed_env(&row.capabilities);
        let allow_outbound = serde_json::from_value::<BTreeSet<String>>(row.egress_policy)?
            .iter()
            .map(|value| hibana_shared::egress::parse_egress_endpoint(value))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow::anyhow!("invalid application egress policy"))?;
        let limits = serde_json::from_value(row.resource_limits)?;
        let config = version_configs::Entity::find()
            .select_only()
            .columns([version_configs::Column::Key, version_configs::Column::Value])
            .filter(version_configs::Column::TenantId.eq(tenant))
            .filter(version_configs::Column::ComponentId.eq(&row.component_id))
            .filter(version_configs::Column::VersionId.eq(&row.version_id))
            .order_by_asc(version_configs::Column::Key)
            .into_tuple::<(String, String)>()
            .all(&tx)
            .await?
            .into_iter()
            .collect();
        tx.commit().await?;
        Ok(ResolvedVersion {
            limits,
            allow_outbound,
            allowed_env,
            config,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL; bash scripts/test-http.sh"]
    async fn execution_configuration_survives_reused_names() {
        let url = std::env::var("HTTP_TEST_DATABASE_URL").unwrap();
        assert!(
            url.ends_with("/hibana_http"),
            "requires disposable test database"
        );
        let pool = hibana_database::postgres::connect(&url, 1, 1)
            .await
            .unwrap();
        // Session-local tables shadow real names, including when another test has
        // populated the disposable DB. All assertions execute the production query.
        pool.execute_unprepared(r#"
          CREATE TEMP TABLE components(id text, tenant_id text, name text, deleted_at timestamptz, egress_policy jsonb NOT NULL DEFAULT '[]');
          CREATE TEMP TABLE component_versions(id text, tenant_id text, component_id text, version text,
            wasm_sha256 text, resource_limits jsonb, capabilities jsonb, deleted_at timestamptz);
          CREATE TEMP TABLE executions(id text, tenant_id text, component_id text, version_id text, status text, http_request boolean, started_at timestamptz);
          CREATE TEMP TABLE version_configs(tenant_id text, component_id text, version_id text, key text, value text);
          INSERT INTO components(id,tenant_id,name,deleted_at) VALUES ('old','t','hello',now()), ('new','t','hello',NULL);
          INSERT INTO component_versions VALUES
            ('old-v','t','old','1','old-hash','{"max_memory_bytes":123}',
             '{"env":["OLD_SECRET"],"net_allow_outbound":["old.example:443"]}',NULL),
            ('new-v','t','new','1','new-hash','{}','{}',NULL);
          INSERT INTO executions VALUES ('accepted','t','new','new-v','pending',true,NULL);
          INSERT INTO version_configs VALUES ('t','old','old-v','MESSAGE','old'),('t','new','new-v','MESSAGE','new');
        "#).await.unwrap();
        let repo = ExecutionRepository::new(pool.clone());
        let resolved = repo
            .resolve_execution("t", "accepted", "new-hash")
            .await
            .unwrap();
        assert_eq!(
            resolved.limits.max_memory_bytes,
            ResourceLimits::default().max_memory_bytes
        );
        assert!(resolved.allowed_env.is_empty());
        assert!(resolved.allow_outbound.is_empty());
        assert_eq!(resolved.config["MESSAGE"], "new");
        // Another deployment of the same app must not change an already accepted execution.
        pool.execute_unprepared(r#"
          INSERT INTO component_versions VALUES ('later-v','t','new','2','later-hash','{}',
            '{"env":["API_KEY","API_KEY","bad-name",42],"net_allow_outbound":["api.example:443","api.example:443","bad",null]}',NULL);
          INSERT INTO version_configs VALUES ('t','new','later-v','MESSAGE','later');
          INSERT INTO executions VALUES ('later','t','new','later-v','pending',true,NULL);
          UPDATE components SET egress_policy='["approved.example:443"]' WHERE id='new';
        "#).await.unwrap();
        assert_eq!(
            repo.resolve_execution("t", "accepted", "new-hash")
                .await
                .unwrap()
                .config["MESSAGE"],
            "new"
        );
        let later = repo
            .resolve_execution("t", "later", "later-hash")
            .await
            .unwrap();
        assert_eq!(later.config["MESSAGE"], "later");
        assert_eq!(later.allowed_env, ["API_KEY".to_string()].into());
        assert_eq!(
            later.allow_outbound,
            vec![hibana_shared::egress::EgressEndpoint {
                host: "approved.example".into(),
                port: 443,
            }]
        );
        // Both pinned versions read the live policy. A stale version grant can
        // neither add access nor restore a destination after revocation.
        assert_eq!(
            repo.resolve_execution("t", "accepted", "new-hash")
                .await
                .unwrap()
                .allow_outbound,
            later.allow_outbound
        );
        pool.execute_unprepared("UPDATE components SET egress_policy='[]' WHERE id='new'")
            .await
            .unwrap();
        assert!(repo
            .resolve_execution("t", "later", "later-hash")
            .await
            .unwrap()
            .allow_outbound
            .is_empty());
        for invalid in ["null", "{}", "[42]", "[\"invalid\"]"] {
            pool.execute_unprepared(&format!(
                "UPDATE components SET egress_policy='{invalid}' WHERE id='new'"
            ))
            .await
            .unwrap();
            assert!(repo
                .resolve_execution("t", "later", "later-hash")
                .await
                .is_err());
        }
        pool.execute_unprepared("UPDATE components SET egress_policy='[]' WHERE id='new'")
            .await
            .unwrap();
        assert!(repo
            .resolve_execution("other", "accepted", "new-hash")
            .await
            .is_err());
        assert!(repo
            .resolve_execution("t", "accepted", "old-hash")
            .await
            .is_err());
        assert!(!repo.mark_running("other", "accepted").await.unwrap());
        assert!(repo.mark_running("t", "accepted").await.unwrap());
        assert!(!repo.mark_running("t", "accepted").await.unwrap());
        pool.execute_unprepared("UPDATE executions SET status='running'")
            .await
            .unwrap();
        assert!(repo
            .resolve_execution("t", "accepted", "new-hash")
            .await
            .is_err());
        pool.close().await.unwrap();
    }
}
